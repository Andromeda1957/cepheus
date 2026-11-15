use super::*;
pub(super) fn start(
    service: &mut Service,
    client: u64,
    request: u64,
    plugin: &str,
    command: &str,
    args: &Value,
) -> bool {
    if plugin != "agent-extensions" {
        return false;
    }
    if command == "skills" {
        service.sync_skills(None);
        service.plugin_reply(client, request, serde_json::json!({"started":true}), None);
        return true;
    }
    if command != "install" {
        return false;
    }
    let mut args = args.clone();
    let confirm = args["confirm"].as_bool().unwrap_or(false);
    args.as_object_mut().unwrap().remove("confirm");
    args["dry_run"] = Value::Bool(!confirm);
    let spec = match serde_json::from_value::<crate::protocol::InstallRequest>(args) {
        Ok(spec) => spec,
        Err(error) => {
            service.plugin_reply(client, request, Value::Null, Some(error.to_string()));
            return true;
        }
    };
    if service.native.installs >= 2 {
        service.plugin_reply(client, request, Value::Null, Some("Two installs are running here already".into()));
        return true;
    }
    service.native.installs += 1;
    if let Some(counted) = service.extensions.pending.get_mut(&(client, request)) {
        *counted = true;
    }
    let tx = service.tx.clone();
    let home = paths::home();
    let cancel = service.extensions.cancel.clone();
    thread::spawn(move || {
        crate::run::with_cancellation(cancel, || {
            let report = crate::installer::install(&spec, &home);
            let _ = tx.send(Ev::PluginReply {
                client,
                request,
                data: serde_json::json!({"ok":report.ok,"lines":report.lines}),
                error: None,
            });
        })
    });
    true
}
