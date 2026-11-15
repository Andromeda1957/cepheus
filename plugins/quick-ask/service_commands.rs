use super::*;
pub(super) fn start(
    service: &mut Service,
    client: u64,
    request: u64,
    plugin: &str,
    command: &str,
    args: &Value,
) -> bool {
    if (plugin, command) != ("quick-ask", "ask") {
        return false;
    }
    let text = args["text"].as_str().unwrap_or_default().to_string();
    if text.trim().is_empty() || text.contains('\0') || text.chars().count() > crate::ask::MAX_QUESTION {
        service.plugin_reply(client, request, Value::Null, Some("Invalid quick question".into()));
        return true;
    }
    let config = service.opts.config.clone();
    let start = service.opts.start_dir.clone();
    let state_dir = service.opts.state_dir.clone();
    let tx = service.tx.clone();
    let cancel = service.extensions.cancel.clone();
    thread::spawn(move || {
        crate::run::with_cancellation(cancel, || {
            let result = (|| -> Result<Value> {
                let asked = crate::ask::ask_once(&config, &text, &start, &state_dir)?;
                let mut data = serde_json::to_value(&asked)?;
                if let Err(error) = crate::ask::append_history(&state_dir, &asked) {
                    data["history_warning"] =
                        serde_json::json!(format!("Answer shown, but history could not be saved: {error:#}"));
                }
                Ok(data)
            })();
            let (data, error) = match result {
                Ok(data) => (data, None),
                Err(e) => (Value::Null, Some(format!("{e:#}"))),
            };
            let _ = tx.send(Ev::PluginReply { client, request, data, error });
        })
    });
    true
}
