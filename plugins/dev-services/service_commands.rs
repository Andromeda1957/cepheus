use super::*;
pub(super) fn command(
    service: &mut Service,
    plugin: &str,
    command: &str,
    args: &Value,
) -> Option<Result<Value, String>> {
    if plugin != "dev-services" {
        return None;
    }
    Some(match command {
        "status" => serde_json::to_value(service.dev_server_state()).map_err(|e| e.to_string()),
        "set" => {
            let run = match args["operation"].as_str() {
                Some("start" | "on") => true,
                Some("stop" | "off") => false,
                _ => return Some(Err("operation must be start or stop".into())),
            };
            if !run && args["confirm"].as_bool() != Some(true) {
                return Some(Err("Stopping the server needs confirm=true".into()));
            }
            service.toggle_dev_server(0, run);
            serde_json::to_value(service.dev_server_state()).map_err(|e| e.to_string())
        }
        _ => Err("No such development-services command".into()),
    })
}
