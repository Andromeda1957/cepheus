use super::*;
use crate::testsupport::TempDir;
use std::process::{Command, Stdio};
use std::time::Duration;

struct Runtime(std::process::Child);
impl Drop for Runtime {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn fixture() -> (TempDir, Config) {
    let dir = TempDir::new("opencode-native-new");
    dir.write(
        "native-state/model.json",
        json!({"recent":[{"providerID":"fixture","modelID":"remembered"}],"variant":{"fixture/remembered":"max"}})
            .to_string(),
    );
    let program = dir.executable("opencode", r#"#!/usr/bin/python3
import json, sys
from pathlib import Path
root = Path.cwd()
args = sys.argv[1:]
if not args: sys.exit(0)
if args[:2] == ['debug', 'paths']:
    print('state ' + str(root / 'native-state'))
    sys.exit(0)
if args[0] == '--session':
    sys.stdin.buffer.read()
    sys.exit(0)
assert args[0] == 'api'
op = args[1]
with (root / 'operations').open('a') as out: out.write(op + '\n')
location = {'directory':str(root)}
if op == 'session.get':
    sid = args[args.index('--param') + 1].split('=', 1)[1]
    path = root / 'sessions' / sid
    if not path.exists():
        print(json.dumps({'_tag':'SessionNotFoundError', 'sessionID':sid}))
        sys.exit(1)
    print(json.dumps({'data':json.loads(path.read_text())}))
elif op == 'agent.list':
    print(json.dumps({'location':location,'data':[{'id':'helper','mode':'subagent','hidden':False},{'id':'build','mode':'primary','hidden':False}]}))
elif op == 'model.list':
    models = [{'providerID':'fixture','id':'catalog-default','variants':[]},{'providerID':'fixture','id':'remembered','variants':[{'id':'max'}]}]
    if (root / 'large-catalog').exists():
        models.extend({'providerID':'other','id':'catalog-model-' + str(n),'variants':[]} for n in range(2000))
    print(json.dumps({'location':location,'data':models}))
elif op == 'config.get': print('[]')
elif op == 'session.create':
    body = json.loads(args[args.index('--data') + 1])
    folder = root / 'sessions'
    folder.mkdir(exist_ok=True)
    assert not (folder / body['id']).exists()
    with (root / 'creations').open('a') as out: out.write(json.dumps(body) + '\n')
    (folder / body['id']).write_text(json.dumps(body))
    if (root / 'wrong-id').exists(): body['id'] = 'ses_other'
    if (root / 'wrong-cwd').exists(): body['location']['directory'] = str(root.parent)
    if (root / 'wrong-model').exists(): body['model']['id'] = 'catalog-default'
    print(json.dumps({'data':body}))
else: raise AssertionError('Unexpected operation: ' + op)
"#).canonicalize().unwrap();
    let mut config = Config::default();
    config.opencode_command = vec![program.to_string_lossy().into_owned()];
    (dir, config)
}
#[test]
fn fresh_opencode_is_saved_with_native_recent_model_before_attachment_and_owner_text() {
    let (dir, config) = fixture();
    dir.write("large-catalog", "");
    let stop = AtomicBool::new(false);
    let deadline = || Instant::now() + Duration::from_secs(10);
    let first =
        prepare(&config, dir.path(), dir.path(), &StartModel::default(), "Owner New", deadline(), &stop).unwrap();
    let second = prepare(&config, dir.path(), dir.path(), &StartModel::default(), "", deadline(), &stop).unwrap();
    assert_ne!(first.conversation, second.conversation);
    let creations: Vec<Value> = std::fs::read_to_string(dir.path().join("creations"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(creations.len(), 2);
    assert_eq!(creations[0]["title"], "Owner New");
    assert!(creations[1].get("title").is_none(), "unnamed sessions retain native automatic titles");
    for created in &creations {
        assert_eq!(created["model"], json!({"providerID":"fixture","id":"remembered","variant":"max"}));
        assert_eq!(created["agent"], "build");
        assert_eq!(created["location"]["directory"], json!(dir.path().canonicalize().unwrap()));
    }
    let runtime = Runtime(
        Command::new(&config.opencode_command[0])
            .args(["--session", &first.conversation])
            .current_dir(dir.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    first.verify(runtime.0.id(), deadline(), &stop).unwrap();
    assert!(
        second.verify(runtime.0.id(), Instant::now() + Duration::from_millis(200), &stop).is_err(),
        "a fresh conversation must not bind to another conversation's TUI"
    );
    let operations = std::fs::read_to_string(dir.path().join("operations")).unwrap();
    assert!(!operations.lines().any(|operation| operation == "session.prompt"));
    assert_eq!(operations.lines().filter(|operation| *operation == "session.create").count(), 2);
}
#[test]
fn fresh_opencode_rejects_wrong_native_receipts_without_recreating_or_prompting() {
    for scenario in ["wrong-id", "wrong-cwd", "wrong-model"] {
        let (dir, config) = fixture();
        dir.write(scenario, "");
        assert!(
            prepare(
                &config,
                dir.path(),
                dir.path(),
                &StartModel::default(),
                "Owner New",
                Instant::now() + Duration::from_secs(10),
                &AtomicBool::new(false)
            )
            .is_err(),
            "{scenario}"
        );
        let operations = std::fs::read_to_string(dir.path().join("operations")).unwrap();
        assert_eq!(operations.lines().filter(|operation| *operation == "session.create").count(), 1);
        assert!(!operations.lines().any(|operation| operation == "session.prompt"));
    }
}
#[test]
fn opencode_uses_agent_then_latest_configuration_then_recent_and_respects_saved_default_variant() {
    let models = json!({"data":[{"providerID":"fixture","id":"recent","variants":[{"id":"max"}]},{"providerID":"fixture","id":"configured","variants":[{"id":"max"}]},{"providerID":"fixture","id":"agent","variants":[{"id":"max"}]}]});
    let prefs = json!({"recent":[{"providerID":"missing","modelID":"gone"},{"providerID":"fixture","modelID":"recent"}],"variant":{"fixture/configured":"default"}});
    let documents = json!([{"type":"document","info":{"model":"fixture/recent"}},{"type":"document","info":{"model":{"providerID":"fixture","model":"configured","variant":"max"}}}]);
    let agents = json!({"data":[{"id":"build","mode":"primary","hidden":false}]});
    let (_, configured) = selection(&agents, &models, &documents, &prefs, &StartModel::default()).unwrap();
    assert_eq!(configured, json!({"providerID":"fixture","id":"configured"}));
    let mut agent_override = agents.clone();
    agent_override["data"][0]["model"] = json!({"providerID":"fixture","id":"agent","variant":"max"});
    let (_, agent) = selection(&agent_override, &models, &documents, &prefs, &StartModel::default()).unwrap();
    assert_eq!(agent, json!({"providerID":"fixture","id":"agent","variant":"max"}));
    let explicit = StartModel { model: Some("fixture/recent".into()), effort: Some("max".into()) };
    let (_, selected) = selection(&agent_override, &models, &documents, &prefs, &explicit).unwrap();
    assert_eq!(selected, json!({"providerID":"fixture","id":"recent","variant":"max"}));
}
