//! What the deck starts each agent program with when a start names no model or effort.
//!
//! `:opus` or `:gpt-6.1-sol` saves a program's model, `:xhigh` its effort. Every fresh session the service
//! starts fills what its start left unset from these settings, so the New dialog, `:claude`, the phone, the
//! queue and scripts all follow them; a start sent on to another host carries this deck's settings. They are
//! the deck's own, in its state folder: the programs' settings files, and sessions started outside the deck,
//! keep their own defaults.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use super::models::{Model, ToolModels};
use crate::protocol::Tool;
use crate::service::{valid_effort, valid_model_name, StartModel};

/// The built-in plugin's catalog id.
pub const PLUGIN: &str = "launch-settings";

/// One program's saved choices; an unset one leaves the program's own.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Launch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
}

/// Every program's saved choices, by the key its model list uses (`claude`,
/// `codex`, a provider's id).
pub type Settings = BTreeMap<String, Launch>;

/// The settings file in the service's state folder.
pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join("launch.json")
}

/// The saved settings; none when the file is missing. A file that does not
/// parse is an error rather than no settings, so a start never quietly runs
/// on a model the owner did not choose.
pub fn load(path: &Path) -> Result<Settings> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Settings::new()),
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let unreadable = || format!("{} holds no valid launch settings; fix it or run :launch reset", path.display());
    let settings: Settings = serde_json::from_slice(&bytes).with_context(unreadable)?;
    let valid = settings.iter().all(|(tool, launch)| {
        crate::extensions::valid_id(tool)
            && launch.model.as_deref().is_none_or(valid_model_name)
            && launch.effort.as_deref().is_none_or(valid_effort)
    });
    if !valid {
        bail!(unreadable());
    }
    Ok(settings)
}

/// Replace the file atomically; no settings removes it.
pub fn save(path: &Path, settings: &Settings) -> Result<()> {
    if settings.is_empty() {
        return match fs::remove_file(path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                Err(error).with_context(|| format!("removing {}", path.display()))
            }
            _ => Ok(()),
        };
    }
    let folder = path.parent().context("the launch settings file has no folder")?;
    fs::create_dir_all(folder)?;
    let staging = folder.join(format!(".launch-{}.json", std::process::id()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&staging)?;
        file.write_all(&serde_json::to_vec_pretty(settings)?)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::set_permissions(&staging, fs::Permissions::from_mode(0o600))?;
        fs::rename(&staging, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staging);
    }
    result.with_context(|| format!("saving {}", path.display()))
}

/// The model and effort a fresh start of `tool` runs with: what the start
/// names, else what is saved for the program. `tools` lists the programs'
/// choices with their own defaults; it is read only to check that a saved
/// effort suits the model, which a start naming another model can break.
/// Such an effort is left to the program, since Codex refuses one its model
/// does not offer.
pub fn resolve(
    tool: Tool,
    start: &StartModel,
    settings: &Settings,
    tools: impl FnOnce() -> Vec<ToolModels>,
) -> StartModel {
    let Some(saved) = settings.get(tool.wire_key()) else { return start.clone() };
    let model = start.model.clone().or_else(|| saved.model.clone());
    let effort = match (&start.effort, &saved.effort) {
        (Some(effort), _) => Some(effort.clone()),
        // An opencode effort is a variant of a model, so it needs one.
        (None, Some(_)) if tool == Tool::OpenCode && model.is_none() => None,
        (None, Some(effort)) => {
            let tools = tools();
            let offered = tools.iter().find(|t| t.tool == tool.wire_key()).map(|t| efforts(t, model.as_deref()));
            match offered {
                Some(list) if !list.is_empty() && !list.contains(effort) => None,
                _ => Some(effort.clone()),
            }
        }
        (None, None) => None,
    };
    StartModel { model, effort }
}

/// Show the saved choices as the programs' defaults, as the New dialog and
/// the phone's model pill name what "default" starts.
pub fn overlay(tools: &mut [ToolModels], settings: &Settings) {
    for t in tools {
        if let Some(saved) = settings.get(t.tool) {
            if saved.model.is_some() {
                t.default_model = saved.model.clone();
            }
            if saved.effort.is_some() {
                t.default_effort = saved.effort.clone();
            }
        }
    }
}

/// The efforts `model` takes: its own, else its program's. No model is the
/// program's default one.
fn efforts<'a>(t: &'a ToolModels, model: Option<&str>) -> &'a [String] {
    match find_model(t, model.or(t.default_model.as_deref())) {
        Some(m) if !m.efforts.is_empty() => &m.efforts,
        _ => &t.efforts,
    }
}

fn find_model<'a>(t: &'a ToolModels, id: Option<&str>) -> Option<&'a Model> {
    let id = id?;
    t.models.iter().find(|m| m.id.eq_ignore_ascii_case(id))
}

/// The model a program starts on: the saved one, else its own default.
fn launch_model<'a>(t: &'a ToolModels, settings: &'a Settings) -> Option<&'a str> {
    settings.get(t.tool).and_then(|s| s.model.as_deref()).or(t.default_model.as_deref())
}

fn label(t: &ToolModels) -> String {
    t.label
        .clone()
        .unwrap_or_else(|| Tool::builtin(t.tool).map_or_else(|| t.tool.to_string(), |tool| tool.label().into()))
}

/// Programs that have models or efforts to choose.
fn choosable(tools: &[ToolModels]) -> impl Iterator<Item = &ToolModels> {
    tools.iter().filter(|t| !t.models.is_empty() || !t.efforts.is_empty())
}

/// Every effort word any program offers, in their own order.
fn all_efforts(tools: &[ToolModels]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in tools {
        for e in t.efforts.iter().chain(t.models.iter().flat_map(|m| m.efforts.iter())) {
            if !out.contains(e) {
                out.push(e.clone());
            }
        }
    }
    out
}

/// The program a word names, among those with choices.
fn program<'a>(tools: &'a [ToolModels], word: &str) -> Option<Result<&'a ToolModels, String>> {
    let tool = Tool::parse(word)?;
    Some(
        choosable(tools)
            .find(|t| t.tool == tool.wire_key())
            .ok_or_else(|| format!("{} has no models or efforts to choose here", tool.label())),
    )
}

fn joined(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// "New Claude sessions start on opus at xhigh effort."
fn describe(programs: &[String], launch: &Launch) -> String {
    let on = launch.model.as_deref().map(|m| format!(" on {m}")).unwrap_or_default();
    let at = launch.effort.as_deref().map(|e| format!(" at {e} effort")).unwrap_or_default();
    if on.is_empty() && at.is_empty() {
        return format!("New {} sessions start with their own model and effort.", joined(programs));
    }
    format!("New {} sessions start{on}{at}.", joined(programs))
}

/// Apply one `:` command: a model, or an effort, then its optional words.
/// `:opus [effort] [program]` saves that model (and effort) for its
/// program; `:xhigh [program…]` saves the effort for each program named,
/// or for every program whose launch model offers it. Nothing is saved on
/// an error.
pub fn set(words: &[String], tools: &[ToolModels], settings: &mut Settings) -> Result<String, String> {
    let (first, rest) = words.split_first().ok_or("name a model or an effort")?;
    let mut programs: Vec<&ToolModels> = Vec::new();
    let mut effort: Option<String> = None;
    let known_efforts = all_efforts(tools);
    for word in rest {
        if let Some(found) = program(tools, word) {
            let t = found?;
            if !programs.iter().any(|p| p.tool == t.tool) {
                programs.push(t);
            }
        } else if effort.is_none() && known_efforts.iter().any(|e| e.eq_ignore_ascii_case(word)) {
            effort = Some(word.to_ascii_lowercase());
        } else {
            return Err(format!("{word:?} is not a program or an effort"));
        }
    }
    let owners: Vec<(&ToolModels, &Model)> =
        choosable(tools).filter_map(|t| find_model(t, Some(first)).map(|m| (t, m))).collect();
    if !owners.is_empty() {
        let named: Vec<&(&ToolModels, &Model)> =
            owners.iter().filter(|(t, _)| programs.is_empty() || programs.iter().any(|p| p.tool == t.tool)).collect();
        let (t, m) = match named.as_slice() {
            [one] if programs.len() <= 1 => **one,
            [] => {
                return Err(format!(
                    "{} has no model {first}",
                    joined(&programs.iter().map(|p| label(p)).collect::<Vec<_>>())
                ))
            }
            [_] => return Err("a model belongs to one program".into()),
            many => {
                let names: Vec<String> = many.iter().map(|(t, _)| label(t)).collect();
                return Err(format!(
                    "{first} is a model of {}; name the program, as :{first} {}",
                    joined(&names),
                    many[0].0.tool
                ));
            }
        };
        let saved = settings.get(t.tool).cloned().unwrap_or_default();
        let given = effort.is_some();
        let effort = effort.or(saved.effort);
        let offered = efforts(t, Some(&m.id));
        if let Some(e) = &effort {
            if !offered.is_empty() && !offered.contains(e) {
                let choices = offered.join("|");
                return Err(if given {
                    format!("{} offers {}", m.label, offered.join(", "))
                } else {
                    format!(
                        "{} does not offer the saved {e} effort; name one it does, as :{} <{choices}>",
                        m.label, m.id
                    )
                });
            }
        }
        let launch = Launch { model: Some(m.id.clone()), effort };
        let text = describe(&[label(t)], &launch);
        settings.insert(t.tool.to_string(), launch);
        return Ok(text);
    }
    let Some(e) = known_efforts.iter().find(|e| e.eq_ignore_ascii_case(first)).cloned() else {
        return Err(format!("{first:?} is not a model or an effort; :launch lists them"));
    };
    if effort.is_some() {
        return Err("give one effort".into());
    }
    let targets: Vec<&ToolModels> = if programs.is_empty() {
        let found: Vec<&ToolModels> =
            choosable(tools).filter(|t| efforts(t, launch_model(t, settings)).contains(&e)).collect();
        if found.is_empty() {
            return Err(format!("no program offers {e} effort with the model it starts on"));
        }
        found
    } else {
        for t in &programs {
            let offered = efforts(t, launch_model(t, settings));
            // opencode's efforts are its models' variants, and its New
            // refuses one the model it picks lacks, so the effort needs a
            // model named.
            if t.tool == Tool::OpenCode.wire_key() && launch_model(t, settings).is_none() {
                return Err(format!("{} has no model saved here; name one with the effort, as :<model> {e}", label(t)));
            }
            if !offered.is_empty() && !offered.contains(&e) {
                let on = launch_model(t, settings).map(|m| format!(" on {m}")).unwrap_or_default();
                return Err(format!("{}{on} offers {}", label(t), offered.join(", ")));
            }
        }
        programs
    };
    let names: Vec<String> = targets.iter().map(|t| label(t)).collect();
    for t in targets {
        settings.entry(t.tool.to_string()).or_default().effort = Some(e.clone());
    }
    Ok(format!("New {} sessions start at {e} effort.", joined(&names)))
}

/// `:launch reset [program] [model|effort]`: forget saved choices, so the
/// programs start with their own again.
pub fn reset(words: &[String], tools: &[ToolModels], settings: &mut Settings) -> Result<String, String> {
    let mut chosen: Option<&ToolModels> = None;
    let mut field: Option<&str> = None;
    for word in words {
        match word.as_str() {
            "model" | "effort" if field.is_none() => field = Some(word),
            _ if chosen.is_none() => {
                chosen = Some(program(tools, word).unwrap_or_else(|| Err(format!("{word:?} is not a program")))?)
            }
            _ => return Err(format!("{word:?} is one word too many")),
        }
    }
    let targets: Vec<String> = match chosen {
        Some(t) => vec![t.tool.to_string()],
        None => settings.keys().cloned().collect(),
    };
    for key in &targets {
        if let Some(saved) = settings.get_mut(key) {
            match field {
                Some("model") => saved.model = None,
                Some(_) => saved.effort = None,
                None => *saved = Launch::default(),
            }
        }
    }
    settings.retain(|_, saved| saved != &Launch::default());
    let what = field.map_or("model and effort".to_string(), str::to_string);
    Ok(match chosen {
        Some(t) => format!("New {} sessions start with {}'s own {what} again.", label(t), label(t)),
        None => format!("Every program starts with its own {what} again."),
    })
}

/// `:launch`: what each program starts with, and how to change it.
pub fn status(tools: &[ToolModels], settings: &Settings) -> Vec<String> {
    let mut lines = Vec::new();
    let mut models: Vec<String> = Vec::new();
    for t in choosable(tools) {
        let saved = settings.get(t.tool).cloned().unwrap_or_default();
        let name = label(t);
        let value = |saved: &Option<String>, own: &Option<String>| match (saved, own) {
            (Some(v), _) => format!("{v} (saved)"),
            (None, Some(v)) => format!("{v} ({name}'s own)"),
            (None, None) => format!("{name}'s own"),
        };
        lines.push(format!(
            "{name}: model {} · effort {}",
            value(&saved.model, &t.default_model),
            value(&saved.effort, &t.default_effort)
        ));
        if !t.models.is_empty() {
            // opencode lists over a hundred; `Tab` completes the rest.
            const SHOWN: usize = 12;
            let ids: Vec<String> = t.models.iter().take(SHOWN).map(|m| format!(":{}", m.id)).collect();
            let more = match t.models.len().saturating_sub(SHOWN) {
                0 => String::new(),
                n => format!(" … and {n} more (Tab completes them)"),
            };
            models.push(format!("  {name}: {}{more}", ids.join(" ")));
        }
    }
    if lines.is_empty() {
        lines.push("No program here has models or efforts to choose.".into());
        return lines;
    }
    lines.push(String::new());
    lines.push(":<model> [effort] [program]  start that program on the model".into());
    lines.extend(models);
    let efforts: Vec<String> = all_efforts(tools).iter().map(|e| format!(":{e}")).collect();
    lines.push(":<effort> [program…]  start at that effort; alone, every program whose model offers it".into());
    lines.push(format!("  {}", efforts.join(" ")));
    lines.push(":launch reset [program] [model|effort]  back to the programs' own".into());
    lines
}

/// The `:` words these settings add, each with what its first argument
/// completes to: a model's efforts and program, or the programs.
pub fn words(tools: &[ToolModels]) -> Vec<(String, Vec<String>)> {
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for t in choosable(tools) {
        for m in &t.models {
            let mut after: Vec<String> = efforts(t, Some(&m.id)).to_vec();
            after.push(t.tool.to_string());
            out.push((m.id.clone(), after));
        }
    }
    let programs: Vec<String> = choosable(tools).map(|t| t.tool.to_string()).collect();
    for e in all_efforts(tools) {
        out.push((e, programs.clone()));
    }
    out.retain(|(word, _)| !crate::actions::is_builtin(word));
    out
}

#[cfg(test)]
#[path = "../../src/tests/launch_settings.rs"]
mod tests;
