use super::*;
pub(super) fn command(
    service: &mut Service,
    plugin: &str,
    command: &str,
    args: &Value,
) -> Option<Result<Value, String>> {
    if plugin != "security-scans" {
        return None;
    }
    Some(match command {
        "list" => Ok(serde_json::json!({"items":service.scan_infos()})),
        "start" => {
            let scope = args["scope"].as_str().filter(|s| !s.is_empty()).map(str::to_string);
            service.start_scan(0, scope);
            Ok(serde_json::json!({"started":true}))
        }
        _ => Err("No such security-scans command".into()),
    })
}
