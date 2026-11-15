//! Configure future service startup without disturbing sessions running now.

use std::fs;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::{config::Config, hosts::setup, paths, util};

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Off,
    Login,
    Boot,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Login => "desktop login",
            Self::Boot => "boot",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    pub mode: Mode,
    pub delay_secs: u64,
    pub restart_secs: u64,
    pub stop_timeout_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self { mode: Mode::Off, delay_secs: 0, restart_secs: 5, stop_timeout_secs: 30 }
    }
}

impl Settings {
    pub fn sanitized(mut self) -> Self {
        self.delay_secs = self.delay_secs.min(3600);
        self.restart_secs = self.restart_secs.clamp(1, 300);
        self.stop_timeout_secs = self.stop_timeout_secs.clamp(1, 300);
        self
    }
}

const USAGE: &str = "usage: cepheus startup status|enable [--boot|--login]|disable|apply\n\
    enable prepares future startup (boot by default); it leaves current sessions running\n\
    apply uses startup settings from config.json; login means graphical desktop login";
pub(crate) const UNIT_MARKER: &str = "# Managed by cepheus startup";
/// Service settings that keep systemd from killing the deck and the agent
/// sessions it hosts . Every unit that runs the service
/// carries them.
/// - session.slice is outside app.slice, the slice desktops such as Omarchy
///   hand to systemd-oomd as kill candidates; there the deck is ineligible
///   for both its memory-pressure and its swap kills.
/// - omit keeps it out of oomd's memory-pressure candidates where a parent
///   such as user@.service is monitored instead.
/// - OOMPolicy=continue keeps the service running when the kernel kills one
///   process inside it; the default stops the whole unit, every session too.
/// - The kernel score is lowered as far as the user manager allows: an
///   unprivileged manager keeps it at its own value, and systemd skips the
///   rest without failing the start.
pub(crate) const OOM_PROTECTION: &str =
    "Slice=session.slice\nManagedOOMPreference=omit\nOOMPolicy=continue\nOOMScoreAdjust=-1000\n";
const MAX_FILE: u64 = 1024 * 1024;

pub fn run(args: &[String]) -> i32 {
    let result = (|| {
        let (cfg, error) = Config::load();
        if let Some(error) = error {
            bail!("config: {error}");
        }
        Controller::local(&cfg)?.execute(args, cfg.startup)
    })();
    match result {
        Ok(message) => {
            println!("{message}");
            0
        }
        Err(error) => {
            eprintln!("cepheus startup: {error:#}");
            1
        }
    }
}

/// Prefer the configured manager even while its startup delay is running.
/// A missing installation keeps the ordinary manual-start behavior available.
pub(crate) fn request_start(cfg: &Config) -> Result<Option<Duration>> {
    if cfg.startup.mode == Mode::Off {
        return Ok(None);
    }
    Controller::local(cfg)?.request_start(&cfg.startup)
}

struct Controller {
    home: PathBuf,
    config: PathBuf,
    state: PathBuf,
    units: PathBuf,
    program: PathBuf,
    start_dir: PathBuf,
    path_env: String,
    provider_env: Vec<(String, String)>,
    systemctl: PathBuf,
    loginctl: PathBuf,
    uid: String,
}

struct Unit {
    name: String,
    path: PathBuf,
    present: bool,
    active: bool,
}

impl Controller {
    fn local(cfg: &Config) -> Result<Self> {
        let cwd = std::env::current_dir()?;
        let home = absolute_path(&paths::home(), &cwd);
        let units = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
            .join("systemd/user");
        Ok(Self {
            config: absolute_path(&paths::config_path(), &cwd),
            state: absolute_path(&paths::state_dir(), &cwd),
            program: home.join(".local/bin/cepheus"),
            start_dir: absolute_path(&cfg.resolve_start_dir()?, &home),
            path_env: std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin:/bin".into()),
            provider_env: startup_paths()?,
            home,
            units: absolute_path(&units, &cwd),
            systemctl: "systemctl".into(),
            loginctl: "loginctl".into(),
            // SAFETY: geteuid has no preconditions.
            uid: unsafe { libc::geteuid() }.to_string(),
        })
    }

    fn execute(&self, args: &[String], settings: Settings) -> Result<String> {
        let action = args.first().map(String::as_str).unwrap_or("status");
        let mut settings = settings.sanitized();
        match action {
            "status" if args.len() <= 1 => self.status(&settings),
            "enable" => {
                settings.mode = match args.get(1).map(String::as_str) {
                    None | Some("--boot") if args.len() <= 2 => Mode::Boot,
                    Some("--login") if args.len() == 2 => Mode::Login,
                    _ => bail!("{USAGE}"),
                };
                self.save_and_apply(settings)
            }
            "disable" if args.len() == 1 => {
                settings.mode = Mode::Off;
                self.save_and_apply(settings)
            }
            "apply" if args.len() == 1 => self.apply(&settings),
            "help" | "--help" | "-h" => Ok(USAGE.into()),
            _ => bail!("{USAGE}"),
        }
    }

    fn save_and_apply(&self, settings: Settings) -> Result<String> {
        let (config, body, comments) = self.updated_config(&settings)?;
        let result = self.apply(&settings)?;
        util::write_atomic(&config, &body).context("saving startup settings")?;
        let note = if comments { " Comments and trailing commas were normalized to JSON." } else { "" };
        Ok(format!("{result}\nSaved startup settings in {}.{note}", config.display()))
    }

    fn updated_config(&self, settings: &Settings) -> Result<(PathBuf, Vec<u8>, bool)> {
        let path = match fs::symlink_metadata(&self.config) {
            Ok(meta) if meta.file_type().is_symlink() => {
                fs::canonicalize(&self.config).context("resolving config link")?
            }
            _ => self.config.clone(),
        };
        let bytes = match fs::metadata(&path) {
            Ok(meta) if meta.is_file() && meta.len() <= MAX_FILE => fs::read(&path)?,
            Ok(_) => bail!("configuration must be a regular file no larger than 1 MiB"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => b"{}".to_vec(),
            Err(error) => return Err(error.into()),
        };
        let stripped = crate::config::strip_comments(&bytes);
        let mut value: serde_json::Value = serde_json::from_slice(&stripped).context("parsing config before update")?;
        let map = value.as_object_mut().context("configuration must be a JSON object")?;
        let startup = map.entry("startup").or_insert_with(|| serde_json::json!({}));
        let startup = startup.as_object_mut().context("startup configuration must be a JSON object")?;
        if let serde_json::Value::Object(settings) = serde_json::to_value(settings)? {
            startup.extend(settings);
        }
        let mut body = serde_json::to_vec_pretty(&value)?;
        body.push(b'\n');
        Ok((path, body, stripped != bytes))
    }

    fn systemctl(&self, args: &[&str]) -> Result<Output> {
        let mut command = Command::new(&self.systemctl);
        command.args(["--user", "--no-pager", "--no-ask-password"]).args(args);
        run_command(command)
    }

    fn check_systemctl(&self, args: &[&str]) -> Result<()> {
        let output = self.systemctl(args)?;
        if !output.status.success() {
            bail!("systemctl {}: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
        }
        Ok(())
    }

    fn unit(&self, name: &str) -> Result<Unit> {
        let path = self.units.join(name);
        let output = self.systemctl(&["show", name, "--property=LoadState,FragmentPath,ActiveState"])?;
        let properties = String::from_utf8_lossy(&output.stdout);
        let value = |key: &str| properties.lines().find_map(|line| line.strip_prefix(key)).unwrap_or("");
        let fragment = value("FragmentPath=");
        let present = fs::symlink_metadata(&path).is_ok() || value("LoadState=") != "not-found" && !fragment.is_empty();
        if !output.status.success() && value("LoadState=") != "not-found" {
            bail!("could not inspect {name}: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        if present {
            if !fragment.is_empty() && fs::canonicalize(fragment).ok() != fs::canonicalize(&path).ok() {
                bail!("{name} is provided outside {}; refusing to shadow it", self.units.display());
            }
            let meta = fs::symlink_metadata(&path).context("reading existing service metadata")?;
            if !meta.is_file() || meta.uid().to_string() != self.uid || meta.len() > MAX_FILE {
                bail!("{} is not an owner-controlled regular unit", path.display());
            }
            let text = fs::read_to_string(&path)?;
            let compatible = [self.program.clone(), self.home.join(".local/bin/agent-deck")].iter().any(|exe| {
                // legacy-name
                let unquoted = format!("{} service", exe.display());
                let quoted = unit_quote(&exe.to_string_lossy(), true).map(|exe| format!("{exe} service"));
                text.lines().any(|line| {
                    line.trim()
                        .strip_prefix("ExecStart=")
                        .is_some_and(|start| start == unquoted || quoted.as_ref().is_ok_and(|quoted| start == quoted))
                })
            });
            if !compatible {
                bail!("{} does not run the installed Cepheus service; leaving it unchanged", path.display());
            }
        }
        Ok(Unit { name: name.into(), path, present, active: value("ActiveState=") == "active" })
    }

    fn units(&self) -> Result<Vec<Unit>> {
        [setup::UNIT, setup::LEGACY_UNIT].iter().map(|name| self.unit(name)).collect()
    }

    fn linger(&self) -> Result<bool> {
        let mut command = Command::new(&self.loginctl);
        command.args(["--no-pager", "--no-ask-password", "show-user", &self.uid, "--property=Linger", "--value"]);
        let output = run_command(command)?;
        if !output.status.success() {
            bail!("could not inspect boot startup: {}", String::from_utf8_lossy(&output.stderr).trim());
        }
        match String::from_utf8_lossy(&output.stdout).trim() {
            "yes" => Ok(true),
            "no" => Ok(false),
            _ => bail!("loginctl returned no valid lingering state"),
        }
    }

    fn status(&self, settings: &Settings) -> Result<String> {
        let units = self.units()?;
        let mut result = format!("Configured startup: {}", settings.mode.label());
        for unit in units.iter().filter(|unit| unit.present) {
            let enabled = self.systemctl(&["is-enabled", &unit.name])?;
            let enabled = String::from_utf8_lossy(&enabled.stdout).trim().to_string();
            result.push_str(&format!(
                "\n{}: {}, {}",
                unit.name,
                enabled,
                if unit.active { "running" } else { "inactive" }
            ));
        }
        if !units.iter().any(|unit| unit.present) {
            result.push_str("\nNo managed service unit is installed.");
        }
        result.push_str(&format!("\nBoot user services: {}", if self.linger()? { "enabled" } else { "off" }));
        Ok(result)
    }

    fn request_start(&self, settings: &Settings) -> Result<Option<Duration>> {
        let units = self.units()?;
        let existing: Vec<_> = units.iter().filter(|unit| unit.present).collect();
        if existing.len() > 1 {
            bail!("competing Cepheus startup services are installed");
        }
        let Some(unit) = existing.first() else { return Ok(None) };
        self.check_systemctl(&["start", "--no-block", &unit.name])?;
        Ok(Some(Duration::from_secs(settings.delay_secs.min(3600) + 8)))
    }

    fn apply(&self, settings: &Settings) -> Result<String> {
        let units = self.units()?;
        let existing: Vec<&Unit> = units.iter().filter(|unit| unit.present).collect();
        if settings.mode == Mode::Off {
            for unit in existing {
                self.check_systemctl(&["disable", &unit.name])?;
            }
            return Ok("Future startup disabled. Running sessions and existing user lingering are unchanged.".into());
        }
        if existing.len() > 1 {
            bail!("both {} and {} exist; refusing to enable competing services", setup::UNIT, setup::LEGACY_UNIT);
        }
        let meta =
            fs::metadata(&self.program).context("install Cepheus before enabling startup (`cepheus install`)")?;
        if !meta.is_file() || meta.permissions().mode() & 0o111 == 0 {
            bail!("{} is not an executable installed program", self.program.display());
        }
        let unit = existing.first().copied().unwrap_or(&units[0]);
        let body = self.unit_text(settings)?;
        if settings.mode == Mode::Boot && !self.linger()? {
            let mut command = Command::new(&self.loginctl);
            command.args(["--no-ask-password", "enable-linger", &self.uid]);
            let output = run_command(command)?;
            if !output.status.success() {
                bail!(
                    "could not enable startup before login (no password prompt was opened): {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
        }
        fs::create_dir_all(&self.units)?;
        util::ensure_private_dir(&self.state)?;
        util::write_atomic(&unit.path, body.as_bytes()).context("writing service unit")?;
        self.check_systemctl(&["daemon-reload"])?;
        // Moves an existing install between targets, without starting or stopping it.
        self.check_systemctl(&["reenable", &unit.name])?;
        Ok(format!(
            "Startup enabled for the next {} ({}). Current sessions keep running; no service was started or restarted.",
            settings.mode.label(),
            unit.name
        ))
    }

    fn unit_text(&self, settings: &Settings) -> Result<String> {
        let cwd = std::env::current_dir()?;
        let home = absolute_path(&self.home, &cwd);
        let program = absolute_path(&self.program, &cwd);
        let config = absolute_path(&self.config, &cwd);
        let state = absolute_path(&self.state, &cwd);
        let start_dir = absolute_path(&self.start_dir, &home);
        let target = if settings.mode == Mode::Login { "graphical-session.target" } else { "default.target" };
        let mut body = format!(
            "{UNIT_MARKER}\n[Unit]\nDescription=Cepheus agent sessions\nAfter=network-online.target\nStartLimitIntervalSec=0\n\n[Service]\nType=simple\nWorkingDirectory={}\n",
            unit_scalar(&home.to_string_lossy())?
        );
        if settings.delay_secs > 0 {
            body.push_str(&format!("ExecStartPre=/usr/bin/sleep {}\n", settings.delay_secs));
        }
        body.push_str(&format!("ExecStart={} service\n", unit_quote(&program.to_string_lossy(), true)?));
        for (name, value) in [
            ("PATH", self.path_env.clone()),
            ("CEPHEUS_CONFIG", config.to_string_lossy().into_owned()),
            ("CEPHEUS_STATE_DIR", state.to_string_lossy().into_owned()),
            ("CEPHEUS_START_DIR", start_dir.to_string_lossy().into_owned()),
        ]
        .into_iter()
        .chain(self.provider_env.iter().map(|(name, value)| (name.as_str(), value.clone())))
        {
            body.push_str(&format!("Environment={}\n", unit_quote(&format!("{name}={value}"), false)?));
        }
        let log = unit_scalar(&format!("append:{}", state.join("service.log").display()))?;
        body.push_str(&format!(
            "StandardOutput={log}\nStandardError={log}\nRestart=on-failure\nRestartSec={}\nTimeoutStopSec={}\n{}\n[Install]\nWantedBy={target}\n",
            settings.restart_secs, settings.stop_timeout_secs, OOM_PROTECTION
        ));
        Ok(body)
    }
}

fn absolute_path(path: &Path, base: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Only durable path roots are carried into boot; credentials and runtime
/// sockets remain owned by their normal login/session providers.
fn startup_paths() -> Result<Vec<(String, String)>> {
    let cwd = std::env::current_dir()?;
    let absolute =
        |path: PathBuf| if path.is_absolute() { path } else { cwd.join(path) }.to_string_lossy().into_owned();
    let mut values = vec![("CEPHEUS_DATA_DIR".into(), absolute(paths::data_dir()))];
    for suffix in ["SOCKET", "ANTIGRAVITY_DIR"] {
        if let Some(value) = paths::env_var(suffix) {
            values.push((format!("CEPHEUS_{suffix}"), absolute(PathBuf::from(value))));
        }
    }
    for name in ["CLAUDE_CONFIG_DIR", "CODEX_HOME", "GROK_HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "XDG_STATE_HOME"] {
        if let Some(value) = std::env::var_os(name).filter(|v| !v.is_empty()) {
            values.push((name.into(), absolute(PathBuf::from(value))));
        }
    }
    Ok(values)
}

/// Scalar systemd paths do not unquote or unescape; only specifiers expand.
fn unit_scalar(value: &str) -> Result<String> {
    if value.chars().any(char::is_control) || value.ends_with(['\\', ' ']) {
        bail!("service paths cannot contain control characters or end with a space or backslash");
    }
    Ok(value.replace('%', "%%"))
}

/// systemd word quoting: specifiers and command dollars remain literal data.
fn unit_quote(value: &str, command: bool) -> Result<String> {
    if value.chars().any(char::is_control) {
        bail!("service paths and environment values cannot contain control characters");
    }
    let mut escaped = value.replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%");
    if command {
        escaped = escaped.replace('$', "$$");
    }
    Ok(format!("\"{escaped}\""))
}

fn run_command(mut command: Command) -> Result<Output> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running the service manager")?;
    let stdout = child.stdout.take().context("service manager stdout")?;
    let stderr = child.stderr.take().context("service manager stderr")?;
    let read = |stream: Box<dyn Read + Send>| {
        thread::spawn(move || {
            let mut body = Vec::new();
            stream.take(64 * 1024 + 1).read_to_end(&mut body).map(|_| body)
        })
    };
    let out = read(Box::new(stdout));
    let err = read(Box::new(stderr));
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("service manager did not answer within 30 seconds");
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = out.join().map_err(|_| anyhow::anyhow!("service manager stdout reader failed"))??;
    let stderr = err.join().map_err(|_| anyhow::anyhow!("service manager stderr reader failed"))??;
    if stdout.len() > 64 * 1024 || stderr.len() > 64 * 1024 {
        bail!("service manager output exceeded 64 KiB");
    }
    Ok(Output { status, stdout, stderr })
}

#[cfg(test)]
#[path = "tests/startup.rs"]
mod tests;
