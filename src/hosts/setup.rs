//! `cepheus peer setup`: the host's half of `cepheus hosts install`,
//! run on the host over SSH once the hub has copied this binary there. The
//! hub key arrives on standard input, never on a command line. It turns the
//! peer listener on in the host's config, records the key's SHA-256, makes
//! the certificate and starts the service under a systemd user unit, so the
//! host serves its deck from boot. It prints what the hub records as one
//! JSON line.

use std::fs;
use std::io::{self, Read};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::config::Config;
use crate::hosts::{self, peer};
use crate::remote::store;
use crate::{paths, util};

pub const UNIT: &str = "cepheus.service";

/// The unit's name on hosts set up before the rename to Cepheus. Such a host
/// keeps that unit, rewritten to run the new program, so that only one unit
/// ever starts its service.
pub const LEGACY_UNIT: &str = "agent-deck.service"; // legacy-name

#[derive(Debug, Default, PartialEq)]
pub struct SetupArgs {
    pub label: String,
    pub listen: String,
    pub allow: Vec<String>,
}

/// What the host tells the hub, one JSON line on standard output.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct SetupReport {
    pub fingerprint: String,
    pub start_dir: String,
    /// How the service was left: "started", "running" or "manual".
    pub service: String,
}

pub fn parse_args(args: &[String]) -> Result<SetupArgs> {
    let mut out = SetupArgs::default();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = || it.next().cloned().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--label" => out.label = value()?,
            "--listen" => out.listen = value()?,
            "--allow" => out.allow.push(value()?),
            other => bail!("unknown setup option {other:?}"),
        }
    }
    if out.listen.trim().is_empty() {
        bail!("--listen is required");
    }
    if out.allow.is_empty() || out.allow.iter().any(|a| hosts::parse_cidr(a).is_none()) {
        bail!("--allow needs at least one address or CIDR range, and only those");
    }
    Ok(out)
}

/// `current` with the peer listener turned on as `args` says. Every other
/// setting the host's owner made is kept; desktop notifications default to
/// off, since the hub raises them where the owner is.
pub fn merged_config(current: Value, args: &SetupArgs) -> Result<Value> {
    let mut map = match current {
        Value::Object(map) => map,
        Value::Null => Map::new(),
        _ => bail!("the config file is not a JSON object"),
    };
    if !args.label.trim().is_empty() {
        map.insert("host_name".into(), json!(args.label.trim()));
    }
    map.insert("peer_listen".into(), json!(args.listen));
    map.insert("peer_allow".into(), json!(args.allow));
    map.entry("notifications").or_insert(json!(false));
    Ok(Value::Object(map))
}

/// The systemd user unit that runs the service from boot. `exec` keeps the
/// process id across in-place updates (`handover.rs`), so the unit sees one
/// long-running service; its log stays where the deck reads it.
pub fn unit_text(exe: &Path, path_env: &str, log: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=Cepheus: this host's sessions, served to the hub\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} service\n\
         Environment=PATH={path_env}\n\
         StandardOutput=append:{log}\n\
         StandardError=append:{log}\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         {oom}\n\
         [Install]\n\
         WantedBy=default.target\n",
        exe = exe.display(),
        log = log.display(),
        oom = crate::startup::OOM_PROTECTION,
    )
}

pub fn run(args: &[String]) -> Result<()> {
    let args = parse_args(args)?;
    let mut key = String::new();
    io::stdin().read_to_string(&mut key)?;
    let key = key.trim();
    if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("expected the hub key (64 hex digits) on standard input");
    }

    // The settings go to the hosts file beside the config file, which the
    // owner's own file overrides key by key; the ones an older build wrote
    // into the config file move out of it so these take effect.
    let config_path = paths::config_path();
    let mut file = crate::config::read_hosts_file(&config_path).unwrap_or_default();
    let merged = merged_config(Value::Object(std::mem::take(&mut file.settings)), &args)?;
    if let Value::Object(settings) = merged {
        file.settings = settings;
    }
    crate::config::write_hosts_file(&config_path, &file)?;
    crate::config::move_keys_out(&config_path, &["host_name", "peer_listen", "peer_allow"])
        .with_context(|| format!("{} is not valid JSON; fix it and install again", config_path.display()))?;
    let (config, err) = Config::load();
    if let Some(err) = err {
        bail!("config: {err}");
    }
    let start_dir = config.resolve_start_dir()?;

    let state = paths::state_dir();
    let dir = peer::peer_dir(&state);
    peer::record_hub_key(&dir, key)?;
    let identity = store::load_or_create_identity(&dir, &util::hostname())?;
    let service = start_service(&state)?;
    let report =
        SetupReport { fingerprint: identity.fingerprint, start_dir: start_dir.to_string_lossy().into_owned(), service };
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

/// Install and enable the unit, starting the service unless one already
/// runs; that one picks up the listener from the config by itself.
fn start_service(state: &Path) -> Result<String> {
    let running = UnixStream::connect(paths::socket_path()).is_ok();
    if Command::new("systemctl").args(["--user", "is-system-running"]).output().is_err() {
        return Ok(if running { "running" } else { "manual" }.to_string());
    }
    let exe = paths::home().join(".local/bin/cepheus");
    let unit_dir = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| paths::home().join(".config"))
        .join("systemd/user");
    fs::create_dir_all(&unit_dir)?;
    util::ensure_private_dir(state)?;
    let path_env = std::env::var("PATH").unwrap_or_default();
    let unit = unit_text(&exe, &path_env, &state.join("service.log"));
    if unit_dir.join(UNIT).exists() && unit_dir.join(LEGACY_UNIT).exists() {
        bail!("both Cepheus service units exist; refusing to enable competing services");
    }
    let name = if unit_dir.join(LEGACY_UNIT).exists() { LEGACY_UNIT } else { UNIT };
    let unit_path = unit_dir.join(name);
    if fs::read_to_string(&unit_path).is_ok_and(|text| text.starts_with(crate::startup::UNIT_MARKER)) {
        // Enrollment may start a peer now, but must preserve the owner's future
        // startup mode, target and timing choices, including a disabled unit.
        if running {
            return Ok("running".into());
        }
        systemctl(&["start", name])?;
        return Ok("started".into());
    }
    if fs::read_to_string(&unit_path).ok().as_deref() != Some(unit.as_str()) {
        fs::write(&unit_path, unit)?;
    }
    systemctl(&["daemon-reload"])?;
    if running {
        systemctl(&["enable", name])?;
        return Ok("running".to_string());
    }
    systemctl(&["enable", "--now", name])?;
    Ok("started".to_string())
}

fn systemctl(args: &[&str]) -> Result<()> {
    let out = Command::new("systemctl").arg("--user").args(args).output().context("running systemctl")?;
    if !out.status.success() {
        bail!("systemctl --user {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/hosts_setup.rs"]
mod tests;
