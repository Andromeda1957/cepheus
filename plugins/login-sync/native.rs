//! Synchronize Claude and Codex sign-ins across configured agent hosts. A missing or rejected login can be
//! supplied from the coordinator; a newer login for the same account can propagate in either direction.
//! Credential files are replaced only by a newer login for that account.
//!
//! Only sign-in data moves: Claude's `claudeAiOauth` in `.credentials.json` with `oauthAccount` from
//! `.claude.json`, and Codex's `auth.json`. Other settings remain local. Credential values are never printed or
//! logged.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::{paths, util};

#[path = "mirrors.rs"]
mod mirrors;
pub use mirrors::{export_with_settings, import_with_settings, state_with_settings};
#[path = "expiry.rs"]
mod expiry;
pub use expiry::{expiry_of, health, Watcher};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum LoginTool {
    Claude,
    Codex,
}

pub const EVERY: std::time::Duration = std::time::Duration::from_secs(30);

pub const TOOLS: [LoginTool; 2] = [LoginTool::Claude, LoginTool::Codex];
/// Synchronization policy; the existing account/newer-login safeguards always
/// apply, including when the allowed direction is restricted.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub interval_seconds: u64,
    pub direction: Direction,
    pub tools: Vec<LoginTool>,
    /// Three days gives a full two days' notice despite polling delays.
    pub warn_before_seconds: u64,
    /// Other local consumers, such as a native installation beside WSL.
    pub credential_mirrors: Vec<LoginPaths>,
    #[serde(skip)]
    pub accounts: crate::providers::accounts::Settings,
}
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    #[default]
    Bidirectional,
    HubToHost,
    HostToHub,
}
impl Direction {
    pub fn push(self) -> bool {
        self != Self::HostToHub
    }
    pub fn pull(self) -> bool {
        self != Self::HubToHost
    }
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            interval_seconds: EVERY.as_secs(),
            direction: Direction::default(),
            tools: TOOLS.to_vec(),
            warn_before_seconds: 3 * 86_400,
            credential_mirrors: Vec::new(),
            accounts: Default::default(),
        }
    }
}
impl Settings {
    pub fn from_config(config: &crate::config::Config) -> Result<Self> {
        let mut value = match config.plugin_settings.get("login-sync") {
            Some(value) => serde_json::from_value::<Self>(value.clone()).context("invalid login-sync settings")?,
            None => Self::default(),
        };
        if !(1..=3600).contains(&value.interval_seconds) || value.tools.len() > TOOLS.len() {
            bail!("login-sync requires an interval of 1..3600 seconds and at most two tools");
        }
        if !(2 * 86_400 + value.interval_seconds..=30 * 86_400).contains(&value.warn_before_seconds) {
            bail!("login-sync warning must allow at least two days plus one polling interval");
        }
        if value.credential_mirrors.len() > 8
            || value.credential_mirrors.iter().any(|p| !p.claude_dir.is_absolute() || !p.codex_dir.is_absolute())
        {
            bail!("login-sync accepts at most eight credential mirrors with absolute directories");
        }
        value.accounts = crate::providers::accounts::Settings::from_config(config)?;
        Ok(value)
    }
}

impl LoginTool {
    pub fn provider(self) -> crate::protocol::Tool {
        match self {
            Self::Claude => crate::protocol::Tool::Claude,
            Self::Codex => crate::protocol::Tool::Codex,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            LoginTool::Claude => "Claude",
            LoginTool::Codex => "Codex",
        }
    }
}

/// Where a machine keeps its sign-ins.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginPaths {
    pub claude_dir: PathBuf,
    pub codex_dir: PathBuf,
}

impl LoginPaths {
    pub fn from_env() -> LoginPaths {
        LoginPaths { claude_dir: paths::claude_dir(), codex_dir: paths::codex_dir() }
    }
}

/// Credential location supplied by the local default or a validated profile.
pub trait LoginLocation {
    fn directory(&self, tool: LoginTool) -> &Path;
    fn claude_json(&self) -> PathBuf;
    fn claude_credentials(&self) -> PathBuf {
        self.directory(LoginTool::Claude).join(".credentials.json")
    }
    fn codex_auth(&self) -> PathBuf {
        self.directory(LoginTool::Codex).join("auth.json")
    }
}

impl LoginLocation for LoginPaths {
    fn directory(&self, tool: LoginTool) -> &Path {
        match tool {
            LoginTool::Claude => &self.claude_dir,
            LoginTool::Codex => &self.codex_dir,
        }
    }
    fn claude_credentials(&self) -> PathBuf {
        self.claude_dir.join(".credentials.json")
    }

    /// `~/.claude.json` beside the default `~/.claude`; inside the
    /// directory when `CLAUDE_CONFIG_DIR` moved it.
    fn claude_json(&self) -> PathBuf {
        match (self.claude_dir.file_name(), self.claude_dir.parent()) {
            (Some(name), Some(parent)) if name == ".claude" => parent.join(".claude.json"),
            _ => self.claude_dir.join(".claude.json"),
        }
    }

    fn codex_auth(&self) -> PathBuf {
        self.codex_dir.join("auth.json")
    }
}

/// What a machine's sign-in is, without any of its secrets.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Default)]
pub struct LoginState {
    /// The account it belongs to; none: not signed in.
    pub account: Option<String>,
    /// When it was last renewed, milliseconds since the epoch.
    pub renewed: Option<i64>,
    /// It can no longer renew itself.
    #[serde(default)]
    pub rejected: bool,
}

const MAX_LOGIN_BYTES: u64 = 256 * 1024;

fn read_optional(path: &Path) -> Result<Option<Value>> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("could not inspect credential file"),
    };
    if !meta.is_file() || meta.len() > MAX_LOGIN_BYTES {
        bail!("credential file must be a bounded regular file");
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .context("could not open credential file")?
        .take(MAX_LOGIN_BYTES + 1)
        .read_to_end(&mut bytes)
        .context("could not read credential file")?;
    if bytes.len() as u64 > MAX_LOGIN_BYTES {
        bail!("credential file is too large");
    }
    let value = serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid credential JSON"))?;
    Ok(Some(value))
}

fn read_login(paths: &impl LoginLocation, tool: LoginTool) -> Result<Option<Value>> {
    match tool {
        LoginTool::Claude => {
            let credentials = read_optional(&paths.claude_credentials())?;
            let account = read_optional(&paths.claude_json())?;
            Ok(
                match (
                    credentials.and_then(|c| c.get("claudeAiOauth").cloned()),
                    account.and_then(|c| c.get("oauthAccount").cloned()),
                ) {
                    (Some(oauth), Some(account)) => {
                        Some(serde_json::json!({"claudeAiOauth":oauth,"oauthAccount":account}))
                    }
                    _ => None,
                },
            )
        }
        LoginTool::Codex => read_optional(&paths.codex_auth()),
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// This machine's sign-in for `tool`.
pub fn state(paths: &impl LoginLocation, tool: LoginTool, now_ms: i64) -> LoginState {
    let login = read_login(paths, tool).ok().flatten().unwrap_or(Value::Null);
    state_of(tool, &login, now_ms)
}

/// What a sign-in in `export`'s shape is.
pub fn state_of(tool: LoginTool, login: &Value, now_ms: i64) -> LoginState {
    let text = |v: Option<&Value>| v.and_then(Value::as_str).filter(|t| !t.is_empty()).map(str::to_string);
    match tool {
        LoginTool::Claude => {
            let oauth = login.get("claudeAiOauth");
            let has_token = text(oauth.and_then(|o| o.get("refreshToken"))).is_some();
            let account = text(login.get("oauthAccount").and_then(|a| a.get("accountUuid"))).filter(|_| has_token);
            let renewed = oauth.and_then(|o| o.get("expiresAt")).and_then(Value::as_i64);
            let refresh_until = oauth.and_then(|o| o.get("refreshTokenExpiresAt")).and_then(Value::as_i64);
            LoginState { account, renewed, rejected: refresh_until.is_some_and(|t| t <= now_ms) }
        }
        LoginTool::Codex => {
            let tokens = login.get("tokens");
            let has_token = text(tokens.and_then(|t| t.get("refresh_token"))).is_some();
            let account = text(tokens.and_then(|t| t.get("account_id"))).filter(|_| has_token);
            let renewed = login.get("last_refresh").and_then(Value::as_str).and_then(rfc3339_ms);
            LoginState {
                account,
                renewed,
                rejected: expiry_of(tool, login).refresh_expires_ms.is_some_and(|t| t <= now_ms),
            }
        }
    }
}

/// The sign-in to copy to another machine.
#[allow(dead_code)] // Keep the canonical facade for account-profile consumers and fixtures.
pub fn export(paths: &impl LoginLocation, tool: LoginTool) -> Result<Value> {
    read_login(paths, tool)?.filter(Value::is_object).context("no sign-in here")
}

fn object_at(path: &Path) -> Result<Map<String, Value>> {
    match read_optional(path)? {
        Some(Value::Object(map)) => Ok(map),
        None => Ok(Map::new()),
        _ => bail!("credential settings must be a JSON object"),
    }
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    util::write_atomic(path, text.as_bytes()).with_context(|| format!("writing {}", path.display()))
}

/// Put a sign-in from another machine in place here.
pub fn import(paths: &impl LoginLocation, tool: LoginTool, login: &Value) -> Result<()> {
    match tool {
        LoginTool::Claude => {
            let (Some(oauth), Some(account)) = (login.get("claudeAiOauth"), login.get("oauthAccount")) else {
                bail!("the Claude sign-in is incomplete");
            };
            let creds_path = paths.claude_credentials();
            let mut creds = object_at(&creds_path)?;
            creds.insert("claudeAiOauth".into(), oauth.clone());
            fs::create_dir_all(paths.directory(LoginTool::Claude))?;
            write_json(&creds_path, &Value::Object(creds))?;
            // Claude rewrites this file often; touch it only when the
            // account itself changes.
            let json_path = paths.claude_json();
            let mut json = object_at(&json_path)?;
            if json.get("oauthAccount") != Some(account) {
                json.insert("oauthAccount".into(), account.clone());
                write_json(&json_path, &Value::Object(json))?;
            }
            Ok(())
        }
        LoginTool::Codex => {
            let Some(tokens) = login.get("tokens").filter(|t| t.is_object()) else {
                bail!("the Codex sign-in is incomplete");
            };
            // Only the login moves. The machine's own API key and sign-in
            // mode stay, so another machine cannot switch it to a key of its
            // choosing; a machine without them gets a ChatGPT sign-in's.
            let path = paths.codex_auth();
            let mut auth = object_at(&path)?;
            auth.insert("tokens".into(), tokens.clone());
            match login.get("last_refresh") {
                Some(at) => auth.insert("last_refresh".into(), at.clone()),
                None => auth.remove("last_refresh"),
            };
            auth.entry("auth_mode").or_insert_with(|| Value::from("chatgpt"));
            auth.entry("OPENAI_API_KEY").or_insert(Value::Null);
            fs::create_dir_all(paths.directory(LoginTool::Codex))?;
            write_json(&path, &Value::Object(auth))
        }
    }
}

/// Profile transfers refuse ambiguous files and links outside the selected root.
fn validate_profile_files(profile: &crate::providers::accounts::Profile) -> Result<()> {
    let files = match profile.login_tool() {
        LoginTool::Claude => vec![profile.claude_credentials(), profile.claude_json()],
        LoginTool::Codex => vec![profile.codex_auth()],
    };
    for path in files {
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_symlink() || !metadata.is_file() => {
                bail!("profile credential file must be a regular file")
            }
            Ok(_) if !read_json(&path).is_some_and(|value| value.is_object()) => {
                bail!("profile credential file is unreadable or malformed")
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                bail!("profile credential file is unreadable")
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn export_profile(profile: &crate::providers::accounts::Profile) -> Result<Value> {
    validate_profile_files(profile)?;
    export(profile, profile.login_tool())
}

/// Verify the delivered login against current disk state, rather than an
/// earlier remote status response.
pub fn import_profile(profile: &crate::providers::accounts::Profile, login: &Value, now: i64) -> Result<()> {
    validate_profile_files(profile)?;
    let tool = profile.login_tool();
    let incoming = state_of(tool, login, now);
    let current = state(profile, tool, now);
    if incoming.account.is_none() || incoming.rejected || incoming.renewed.is_none() {
        bail!("profile sign-in is incomplete or rejected");
    }
    // Keep an existing account binding even after its refresh token was
    // removed or became unusable; a profile must never change accounts.
    let current_account = match tool {
        LoginTool::Claude => read_json(&profile.claude_json())
            .and_then(|value| value.get("oauthAccount")?.get("accountUuid")?.as_str().map(str::to_owned)),
        LoginTool::Codex => read_json(&profile.codex_auth())
            .and_then(|value| value.get("tokens")?.get("account_id")?.as_str().map(str::to_owned)),
    }
    .filter(|id| !id.is_empty());
    let current_has_token = match tool {
        LoginTool::Claude => read_json(&profile.claude_credentials())
            .and_then(|value| value.get("claudeAiOauth")?.get("refreshToken")?.as_str().map(str::to_owned)),
        LoginTool::Codex => read_json(&profile.codex_auth())
            .and_then(|value| value.get("tokens")?.get("refresh_token")?.as_str().map(str::to_owned)),
    }
    .is_some_and(|token| !token.is_empty());
    if current_account.is_none() && current_has_token {
        bail!("profile credential identity is unavailable");
    }
    if current_account.is_some() {
        if current_account != incoming.account {
            bail!("profile is signed in to another account");
        }
        if incoming.renewed <= current.renewed {
            bail!("profile already has an equally new or newer sign-in");
        }
    }
    import(profile, tool, login)
}

pub const PROFILE_PROTOCOL: u32 = 14;

/// Which way a sign-in moves between the hub and one host.
#[derive(Debug, PartialEq)]
pub enum Move {
    Stay,
    /// The hub's goes to the host.
    ToHost,
    /// The host's goes to the hub.
    ToHub,
    /// Signed in to different accounts: said once, never overwritten.
    Report,
}

pub fn decide(hub: &LoginState, host: &LoginState) -> Move {
    let Some(hub_account) = &hub.account else {
        // Nothing here to copy, and this machine is written only with a
        // newer login for its own account.
        return Move::Stay;
    };
    let Some(host_account) = &host.account else { return if hub.rejected { Move::Stay } else { Move::ToHost } };
    if hub_account != host_account {
        return if host.rejected && !hub.rejected { Move::ToHost } else { Move::Report };
    }
    match (hub.renewed, host.renewed) {
        (Some(ours), Some(theirs)) if theirs > ours && !host.rejected => Move::ToHub,
        (Some(ours), Some(theirs)) if ours > theirs && !hub.rejected => Move::ToHost,
        (Some(_), None) if !hub.rejected => Move::ToHost,
        (None, Some(_)) if !host.rejected => Move::ToHub,
        _ if host.rejected && !hub.rejected => Move::ToHost,
        _ => Move::Stay,
    }
}

/// `2026-10-31T22:41:55.123456Z` or with an offset, as milliseconds since
/// the epoch.
pub fn rfc3339_ms(text: &str) -> Option<i64> {
    let text = text.trim();
    let num = |s: &str| -> Option<i64> { s.bytes().all(|b| b.is_ascii_digit()).then(|| s.parse().ok()).flatten() };
    // The byte offsets below would split a wider character, and the text
    // comes from another machine's file.
    if !text.is_ascii() || text.len() < 20 || text.as_bytes()[4] != b'-' || text.as_bytes()[7] != b'-' {
        return None;
    }
    let (year, month, day) = (num(&text[0..4])?, num(&text[5..7])?, num(&text[8..10])?);
    if !matches!(text.as_bytes()[10], b'T' | b't' | b' ') || text.as_bytes()[13] != b':' || text.as_bytes()[16] != b':'
    {
        return None;
    }
    let (hour, minute, second) = (num(&text[11..13])?, num(&text[14..16])?, num(&text[17..19])?);
    let mut rest = &text[19..];
    let mut millis = 0;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let padded = format!("{:0<3}", &frac[..digits.min(3)]);
        millis = num(&padded)?;
        rest = &frac[digits..];
    }
    let offset_secs = match rest {
        "Z" | "z" => 0,
        _ if rest.len() == 6 && (rest.starts_with('+') || rest.starts_with('-')) && &rest[3..4] == ":" => {
            let secs = num(&rest[1..3])? * 3600 + num(&rest[4..6])? * 60;
            if rest.starts_with('-') {
                -secs
            } else {
                secs
            }
        }
        _ => return None,
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs;
    Some(secs * 1000 + millis)
}

use crate::hosts::upkeep::{ask, unexpected};
use crate::hosts::wire::{HostReq, HostResp, ToolLogin};

#[cfg(test)]
pub fn sync_logins(
    host: &crate::config::HostConfig,
    stream: &mut std::os::unix::net::UnixStream,
    hub: &crate::hosts::upkeep::HubCtx,
) -> Result<Vec<(LoginTool, Option<String>)>, crate::hosts::upkeep::Broken> {
    sync_with_settings(host, stream, hub, &Settings::default(), PROFILE_PROTOCOL)
}

/// One round of keeping sign-ins current between the hub and the host.
pub fn sync_with_settings(
    host: &crate::config::HostConfig,
    stream: &mut std::os::unix::net::UnixStream,
    hub: &crate::hosts::upkeep::HubCtx,
    settings: &Settings,
    peer_version: u32,
) -> Result<Vec<(LoginTool, Option<String>)>, crate::hosts::upkeep::Broken> {
    let label = host.label();
    let list = match ask(stream, &HostReq::Logins, None)? {
        HostResp::Logins { logins } => logins,
        HostResp::Error { text } => return Ok(vec![(LoginTool::Claude, Some(format!("{label}: {text}")))]),
        other => return Err(unexpected(&other)),
    };
    let now = self::now_ms();
    let mut notes = Vec::new();
    for ToolLogin { tool, state: theirs } in list {
        if !settings.tools.contains(&tool) {
            continue;
        }
        let ours = match self::state_with_settings(&hub.logins, tool, now, settings) {
            Ok(state) => state,
            Err(error) => {
                notes.push((tool, Some(format!("{} login synchronization: {error:#}", tool.label()))));
                continue;
            }
        };
        let name = tool.label();
        let decision = match self::decide(&ours, &theirs) {
            Move::ToHost if !settings.direction.push() => Move::Stay,
            Move::ToHub if !settings.direction.pull() => Move::Stay,
            movement => movement,
        };
        let note = match decision {
            Move::Stay => None,
            Move::ToHost => match self::export_with_settings(&hub.logins, tool, settings) {
                Ok(login) => match ask(stream, &HostReq::PutLogin { tool, login }, None)? {
                    HostResp::Done => {
                        eprintln!("cepheus: copied this machine's {name} sign-in to {}", host.name);
                        None
                    }
                    HostResp::Error { text } => Some(format!("Could not copy the {name} sign-in to {label}: {text}")),
                    other => return Err(unexpected(&other)),
                },
                Err(err) => Some(format!("Could not read this machine's {name} sign-in: {err:#}")),
            },
            Move::ToHub => match ask(stream, &HostReq::GetLogin { tool }, None)? {
                HostResp::Login { login } => {
                    // This machine is written only with a newer login for
                    // its own account, checked on what actually arrived.
                    let incoming = self::state_of(tool, &login, now);
                    if incoming.account.is_some()
                        && incoming.account == ours.account
                        && incoming.renewed > ours.renewed
                        && !incoming.rejected
                    {
                        match self::import_with_settings(&hub.logins, tool, &login, settings) {
                            Ok(()) => {
                                eprintln!("cepheus: took {}'s newer {name} sign-in", host.name);
                                None
                            }
                            Err(err) => Some(format!("Could not take {label}'s newer {name} sign-in: {err:#}")),
                        }
                    } else {
                        None
                    }
                }
                HostResp::Error { text } => Some(format!("Could not read {label}'s {name} sign-in: {text}")),
                other => return Err(unexpected(&other)),
            },
            Move::Report => {
                eprintln!("cepheus: {label}'s {name} is signed in to another account; it is left as it is");
                None
            }
        };
        notes.push((tool, note));
    }
    if !settings.accounts.profiles.is_empty() {
        notes.extend(sync_profiles(host, stream, settings, peer_version)?);
    }
    // Upkeep retains one note per tool, so aggregate profile errors instead
    // of clearing/re-reporting them every sync when several profiles exist.
    Ok(TOOLS
        .into_iter()
        .map(|tool| {
            let messages: Vec<String> =
                notes.iter().filter(|(which, _)| *which == tool).filter_map(|(_, message)| message.clone()).collect();
            (tool, (!messages.is_empty()).then(|| messages.join("; ")))
        })
        .collect())
}

fn sync_profiles(
    host: &crate::config::HostConfig,
    stream: &mut std::os::unix::net::UnixStream,
    settings: &Settings,
    peer_version: u32,
) -> Result<Vec<(LoginTool, Option<String>)>, crate::hosts::upkeep::Broken> {
    let label = host.label();
    if peer_version < PROFILE_PROTOCOL {
        return Ok(vec![(
            LoginTool::Claude,
            Some(format!("{label}: update needed for account profiles (protocol {PROFILE_PROTOCOL})")),
        )]);
    }
    let list = match ask(stream, &HostReq::ProfileLogins, None)? {
        HostResp::ProfileLogins { logins } => logins,
        HostResp::Error { text } => return Ok(vec![(LoginTool::Claude, Some(format!("{label}: {text}")))]),
        other => return Err(unexpected(&other)),
    };
    let now = now_ms();
    let mut notes = Vec::new();
    for (id, profile) in &settings.accounts.profiles {
        let tool = profile.login_tool();
        if !settings.tools.contains(&tool) {
            continue;
        }
        let outcome = (|| -> Result<Option<String>, crate::hosts::upkeep::Broken> {
            let Some(theirs) =
                list.iter().find(|item| item.profile == *id && item.tool == tool).map(|item| &item.state)
            else {
                return Ok(Some(format!("{label}: account profile {id} is not configured for {}", tool.label())));
            };
            let ours = state(profile, tool, now);
            // A profile is never repurposed for another account, including
            // when the stored account is rejected. Default policy stays intact.
            let movement = if ours.account.is_some() && theirs.account.is_some() && ours.account != theirs.account {
                Move::Report
            } else {
                decide(&ours, theirs)
            };
            let result = match movement {
                Move::ToHost if settings.direction.push() => match export_profile(profile) {
                    Ok(login) => {
                        match ask(stream, &HostReq::PutProfileLogin { profile: id.clone(), tool, login }, None)? {
                            HostResp::Done => Ok(()),
                            HostResp::Error { text } => Err(anyhow::anyhow!(text)),
                            other => return Err(unexpected(&other)),
                        }
                    }
                    Err(error) => Err(error),
                },
                Move::ToHub if settings.direction.pull() => {
                    match ask(stream, &HostReq::GetProfileLogin { profile: id.clone(), tool }, None)? {
                        HostResp::Login { login } => import_profile(profile, &login, now),
                        HostResp::Error { text } => Err(anyhow::anyhow!(text)),
                        other => return Err(unexpected(&other)),
                    }
                }
                Move::Report => {
                    eprintln!("cepheus: {label}: account profile {id} is signed in to another account; left unchanged");
                    return Ok(None);
                }
                _ => Ok(()),
            };
            // Import/export errors describe the operation, never credential values.
            Ok(result.err().map(|_| format!("{label}: could not synchronize account profile {id}")))
        })()?;
        notes.push((tool, outcome));
    }
    Ok(notes)
}

#[cfg(test)]
#[path = "../../src/tests/hosts_logins.rs"]
mod tests;

fn read_json(path: &Path) -> Option<Value> {
    read_optional(path).ok().flatten()
}
