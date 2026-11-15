//! Persist a fresh OpenCode session before its first interactive attachment.
use super::Request;
use crate::{config::Config, protocol::Tool, service::StartModel};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Instant;

pub struct Prepared {
    pub conversation: String,
    /// Settings belong to the saved native session, not unsupported TUI flags.
    pub settings: StartModel,
    request: Request,
    agent: String,
    model: Value,
    title: Option<String>,
}

fn api(
    request: &Request,
    operation: &str,
    parameters: &[String],
    body: Option<&Value>,
    deadline: Instant,
    stop: &AtomicBool,
) -> Result<(bool, Value)> {
    let mut args = vec!["api".into(), operation.into()];
    for parameter in parameters {
        args.extend(["--param".into(), parameter.clone()]);
    }
    if let Some(body) = body {
        args.extend(["--data".into(), body.to_string()]);
    }
    // Native catalogs have no pagination; owner-message receipts keep their smaller bound.
    let limit = if ["agent.list", "model.list", "config.get"].contains(&operation) { 1 << 20 } else { 65_536 };
    let (success, output) = request.run_limit(&args, deadline, stop, limit).map_err(anyhow::Error::msg)?;
    let value = serde_json::from_str(&output).context("OpenCode returned an invalid native response")?;
    Ok((success, value))
}
fn location(value: &Value, cwd: &Path) -> bool {
    value["location"]["directory"].as_str().and_then(|directory| Path::new(directory).canonicalize().ok()).as_deref()
        == Some(cwd)
}
fn scoped(request: &Request, operation: &str, deadline: Instant, stop: &AtomicBool) -> Result<Value> {
    let directory = request.cwd.to_str().context("OpenCode directory is not UTF-8")?;
    let parameters = [format!("location[directory]={directory}")];
    let (success, value) = api(request, operation, &parameters, None, deadline, stop)?;
    ensure!(success, "OpenCode could not resolve native New settings");
    // config.get is a priority-ordered document array; other calls include location receipts.
    ensure!(
        if operation == "config.get" { value.is_array() } else { location(&value, &request.cwd) },
        "OpenCode settings belong to another directory"
    );
    Ok(value)
}
fn preferences(request: &Request, deadline: Instant, stop: &AtomicBool) -> Result<Value> {
    let (success, paths) =
        request.run(&["debug".into(), "paths".into()], deadline, stop).map_err(anyhow::Error::msg)?;
    ensure!(success, "OpenCode native state directory is unavailable");
    let state = paths
        .lines()
        .find_map(|line| {
            let (key, value) = line.split_once(char::is_whitespace)?;
            (key == "state").then(|| PathBuf::from(value.trim()))
        })
        .context("OpenCode native state directory is unavailable")?;
    ensure!(state.is_absolute(), "OpenCode state directory is not absolute");
    let path = state.join("model.json");
    let before = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(json!({})),
        Err(error) => return Err(error).context("OpenCode model preferences are unavailable"),
    };
    let owned = |metadata: &std::fs::Metadata| {
        metadata.is_file() && [0, unsafe { libc::geteuid() }].contains(&metadata.uid()) && metadata.mode() & 0o002 == 0
    };
    const LIMIT: u64 = 65_536;
    ensure!(owned(&before) && before.len() <= LIMIT, "OpenCode model preferences are not a bounded owned file");
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path)?;
    let opened = file.metadata()?;
    ensure!(
        owned(&opened) && opened.len() <= LIMIT && (before.dev(), before.ino()) == (opened.dev(), opened.ino()),
        "OpenCode model preferences changed during opening"
    );
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= LIMIT as usize, "OpenCode model preferences exceeded their limit");
    let after = std::fs::symlink_metadata(&path)?;
    ensure!(
        owned(&after)
            && (after.dev(), after.ino()) == (opened.dev(), opened.ino())
            && after.len() == bytes.len() as u64,
        "OpenCode model preferences changed during reading"
    );
    let value: Value = serde_json::from_slice(&bytes).context("OpenCode model preferences are invalid")?;
    ensure!(value.is_object(), "OpenCode model preferences are invalid");
    Ok(value)
}
fn reference(value: &Value, id_key: &str) -> Option<Value> {
    let provider = value["providerID"].as_str().filter(|value| !value.is_empty())?;
    let id = value[id_key].as_str().filter(|value| !value.is_empty())?;
    let mut model = json!({"providerID":provider,"id":id});
    if let Some(variant) = value["variant"].as_str() {
        model["variant"] = json!(variant);
    }
    Some(model)
}
fn named_model(name: &str) -> Option<Value> {
    let (name, variant) = name.split_once('#').map_or((name, None), |(name, variant)| (name, Some(variant)));
    let (provider, id) = name.split_once('/')?;
    if provider.is_empty() || id.is_empty() {
        return None;
    }
    let mut model = json!({"providerID":provider,"id":id});
    if let Some(variant) = variant {
        model["variant"] = json!(variant);
    }
    Some(model)
}
fn same_model(left: &Value, right: &Value) -> bool {
    left["providerID"] == right["providerID"] && left["id"] == right["id"]
}
fn selection(
    agents: &Value,
    models: &Value,
    documents: &Value,
    preferences: &Value,
    settings: &StartModel,
) -> Result<(String, Value)> {
    let catalog = models["data"].as_array().context("OpenCode model catalog is unavailable")?;
    let agent = agents["data"]
        .as_array()
        .context("OpenCode agent catalog is unavailable")?
        .iter()
        .find(|agent| agent["mode"] != "subagent" && agent["hidden"] != true)
        .context("OpenCode has no available primary agent")?;
    let agent_id = agent["id"].as_str().context("OpenCode primary agent has no identity")?.to_owned();
    let available = |model: &Value| catalog.iter().any(|candidate| same_model(candidate, model));
    let agent_model = reference(&agent["model"], "id");
    let configured = documents
        .as_array()
        .context("OpenCode settings are not documents")?
        .iter()
        .rev()
        .find(|document| document["type"] == "document" && !document["info"]["model"].is_null())
        .and_then(|document| {
            let model = &document["info"]["model"];
            model.as_str().and_then(named_model).or_else(|| reference(model, "model"))
        });
    let explicit = settings
        .model
        .as_deref()
        .map(|name| named_model(name).filter(&available).context("The selected OpenCode model is unavailable"))
        .transpose()?;
    // Explicit Deck choices take precedence; otherwise follow the native TUI.
    let mut model = explicit
        .clone()
        .or_else(|| agent_model.clone().filter(&available))
        .or_else(|| configured.clone().filter(&available))
        .or_else(|| {
            preferences["recent"]
                .as_array()
                .and_then(|recent| recent.iter().filter_map(|entry| reference(entry, "modelID")).find(&available))
        })
        .or_else(|| catalog.first().and_then(|entry| reference(entry, "id")))
        .context("OpenCode has no available model")?;
    let key = format!("{}/{}", model["providerID"].as_str().unwrap(), model["id"].as_str().unwrap());
    let explicit_variant =
        settings.effort.as_deref().or_else(|| explicit.as_ref().and_then(|value| value["variant"].as_str()));
    let variant = explicit_variant
        .or_else(|| preferences["variant"][&key].as_str())
        .or_else(|| {
            agent_model.as_ref().filter(|value| same_model(value, &model)).and_then(|value| value["variant"].as_str())
        })
        .or_else(|| {
            configured.as_ref().filter(|value| same_model(value, &model)).and_then(|value| value["variant"].as_str())
        });
    let supported = variant
        .filter(|variant| *variant != "default")
        .filter(|variant| {
            catalog
                .iter()
                .find(|entry| same_model(entry, &model))
                .and_then(|entry| entry["variants"].as_array())
                .is_some_and(|variants| variants.iter().any(|entry| entry["id"].as_str() == Some(*variant)))
        })
        .map(str::to_owned);
    ensure!(
        explicit_variant.is_none_or(|variant| variant == "default" || supported.is_some()),
        "The selected OpenCode variant is unavailable"
    );
    model.as_object_mut().unwrap().remove("variant");
    if let Some(variant) = supported {
        model["variant"] = json!(variant);
    }
    Ok((agent_id, model))
}
pub fn prepare(
    config: &Config,
    cwd: &Path,
    home: &Path,
    settings: &StartModel,
    name: &str,
    deadline: Instant,
    stop: &AtomicBool,
) -> Result<Prepared> {
    let cwd = cwd.canonicalize()?;
    let conversation = crate::providers::other::new_opencode_session_id()?;
    let request = super::request(Tool::OpenCode, config, &conversation, "", &cwd, home)
        .context("This OpenCode launch configuration cannot initialize native New")?;
    let parameters = [format!("sessionID={conversation}")];
    let (exists, value) = api(&request, "session.get", &parameters, None, deadline, stop)?;
    ensure!(
        !exists && value["_tag"] == "SessionNotFoundError" && value["sessionID"].as_str() == Some(&conversation),
        "The fresh OpenCode identity could not be confirmed unused"
    );
    let preferences = preferences(&request, deadline, stop)?;
    let agents = scoped(&request, "agent.list", deadline, stop)?;
    let models = scoped(&request, "model.list", deadline, stop)?;
    let documents = scoped(&request, "config.get", deadline, stop)?;
    let (agent, model) = selection(&agents, &models, &documents, &preferences, settings)?;
    let title = (!name.is_empty()).then(|| name.to_owned());
    let mut body = json!({"id":conversation,"location":{"directory":cwd},"agent":agent,"model":model});
    if let Some(title) = &title {
        body["title"] = json!(title);
    }
    let (success, created) = api(&request, "session.create", &[], Some(&body), deadline, stop)?;
    ensure!(success, "OpenCode refused native New; nothing was retried");
    let prepared = Prepared { conversation, settings: StartModel::default(), request, agent, model, title };
    prepared.saved(&created)?;
    let (success, saved) = api(&prepared.request, "session.get", &parameters, None, deadline, stop)?;
    ensure!(success, "The fresh OpenCode session could not be read back");
    prepared.saved(&saved)?;
    Ok(prepared)
}
impl Prepared {
    fn saved(&self, value: &Value) -> Result<()> {
        let data = &value["data"];
        ensure!(
            data["id"].as_str() == Some(&self.conversation)
                && location(data, &self.request.cwd)
                && data["agent"].as_str() == Some(&self.agent)
                && same_model(&data["model"], &self.model)
                && data["model"]["variant"].as_str() == self.model["variant"].as_str()
                && self.title.as_deref().is_none_or(|title| data["title"].as_str() == Some(title)),
            "The fresh OpenCode session did not preserve its identity, directory, agent or model"
        );
        Ok(())
    }
    pub fn verify(mut self, pid: u32, deadline: Instant, stop: &AtomicBool) -> Result<()> {
        self.request = self.request.pin_runtime(pid);
        while !self.request.runtime_current() {
            ensure!(
                !stop.load(std::sync::atomic::Ordering::SeqCst)
                    && Instant::now() < deadline
                    && self.request.runtime_start.is_some_and(|start| crate::procs::alive_with_start(pid, Some(start))),
                "The fresh OpenCode client does not match its prepared session"
            );
            std::thread::sleep(
                deadline.saturating_duration_since(Instant::now()).min(std::time::Duration::from_millis(20)),
            );
        }
        let parameters = [format!("sessionID={}", self.conversation)];
        let (success, value) = api(&self.request, "session.get", &parameters, None, deadline, stop)?;
        ensure!(success, "The fresh OpenCode session disappeared during attachment");
        self.saved(&value)?;
        ensure!(self.request.runtime_current(), "The fresh OpenCode client changed during attachment");
        Ok(())
    }
}
#[cfg(test)]
#[path = "../../src/tests/opencode_new.rs"]
mod tests;
