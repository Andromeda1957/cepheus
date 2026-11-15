//! The service's side of `:launch` and the model and effort commands: it
//! owns the settings file the starts it makes read.
use super::*;
use crate::providers::launch;
use crate::remote::models;

/// The words a command may carry: the command word and a few arguments.
fn words(args: &Value, limit: usize) -> Result<Vec<String>, String> {
    let list = args["words"].as_array().ok_or("words must be a list")?;
    if list.len() > limit {
        return Err("too many words".into());
    }
    list.iter()
        .map(|w| w.as_str().filter(|w| !w.is_empty() && w.len() <= 100).map(str::to_string))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| "each word must be short text".into())
}

pub(super) fn command(
    service: &mut Service,
    plugin: &str,
    command: &str,
    args: &Value,
) -> Option<Result<Value, String>> {
    if plugin != launch::PLUGIN {
        return None;
    }
    let path = launch::path(&service.opts.state_dir);
    let tools = models::all(&service.model_homes());
    if command == "status" {
        // Antigravity's and opencode's lists change with their releases;
        // looking at them is a reason to fetch them again soon.
        service.refresh_model_lists(std::time::Instant::now(), crate::service::MODEL_LISTS_ASKED);
    }
    let change = |apply: &dyn Fn(&mut launch::Settings) -> Result<String, String>| -> Result<Value, String> {
        let mut settings = launch::load(&path).map_err(|e| format!("{e:#}"))?;
        let message = apply(&mut settings)?;
        launch::save(&path, &settings).map_err(|e| format!("{e:#}"))?;
        Ok(serde_json::json!({"message": message}))
    };
    Some(match command {
        "status" => launch::load(&path)
            .map(|settings| serde_json::json!({"lines": launch::status(&tools, &settings)}))
            .map_err(|e| format!("{e:#}")),
        "set" => words(args, 8).and_then(|words| change(&|settings| launch::set(&words, &tools, settings))),
        // Forgetting everything needs no readable file, so it also clears
        // one that no longer parses.
        "reset" => words(args, 2).and_then(|words| {
            if words.is_empty() {
                let message = launch::reset(&words, &tools, &mut launch::Settings::new())?;
                launch::save(&path, &launch::Settings::new()).map_err(|e| format!("{e:#}"))?;
                return Ok(serde_json::json!({"message": message}));
            }
            change(&|settings| launch::reset(&words, &tools, settings))
        }),
        _ => Err("No such launch-settings command".into()),
    })
}
