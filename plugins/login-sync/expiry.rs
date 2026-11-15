//! Expiry metadata is advisory: access-token expiry is not session expiry.
use super::*;

#[derive(Clone, Copy, Default, Serialize)]
pub struct Expiry {
    pub access_expires_ms: Option<i64>,
    pub refresh_expires_ms: Option<i64>,
}

fn jwt_expiry(token: Option<&Value>) -> Option<i64> {
    let token = token?.as_str()?;
    if token.len() > 32 * 1024 {
        return None;
    }
    let mut pieces = token.split('.');
    pieces.next()?;
    let payload = pieces.next()?;
    if pieces.next()?.is_empty() || pieces.next().is_some() {
        return None;
    }
    let mut payload = payload.replace('-', "+").replace('_', "/");
    while payload.len() % 4 != 0 {
        payload.push('=');
    }
    let bytes = util::unbase64(&payload)?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims.get("exp")?.as_i64()?.checked_mul(1000).filter(|t| *t > 0)
}

pub fn expiry_of(tool: LoginTool, login: &Value) -> Expiry {
    match tool {
        LoginTool::Claude => {
            let oauth = &login["claudeAiOauth"];
            Expiry {
                access_expires_ms: oauth["expiresAt"].as_i64(),
                refresh_expires_ms: oauth["refreshTokenExpiresAt"].as_i64(),
            }
        }
        LoginTool::Codex => Expiry {
            access_expires_ms: jwt_expiry(login["tokens"].get("access_token")),
            refresh_expires_ms: jwt_expiry(login["tokens"].get("refresh_token")),
        },
    }
}

#[derive(Clone, PartialEq)]
struct WarningKey {
    account: Option<String>,
    renewed: Option<i64>,
    deadline: i64,
    refresh: bool,
    expired: bool,
}

fn warning(tool: LoginTool, login: &Value, settings: &Settings, now: i64) -> Option<(WarningKey, String)> {
    let state = state_of(tool, login, now);
    state.account.as_ref()?;
    let expiry = expiry_of(tool, login);
    let (deadline, refresh) = match (expiry.refresh_expires_ms, expiry.access_expires_ms) {
        (Some(deadline), _) => (deadline, true),
        (None, Some(deadline)) => (deadline, false),
        _ => return None,
    };
    if deadline.saturating_sub(now) > settings.warn_before_seconds.saturating_mul(1000).min(i64::MAX as u64) as i64 {
        return None;
    }
    let expired = deadline <= now;
    let hours = deadline.saturating_sub(now).max(0).saturating_add(3_599_999) / 3_600_000;
    let name = tool.label();
    let text = if refresh {
        if expired {
            format!("{name}{RENEWAL_ENDED} Sign in again; the renewed login will be copied to the other hosts.")
        } else {
            format!("{name}{RENEWAL_ENDS}{hours} hours. Refresh the login before then; the renewed login will be copied to the other hosts.")
        }
    } else {
        let when = if expired { "has expired".into() } else { format!("expires in {hours} hours") };
        format!("{name}{ACCESS}{when}. Run {name} to renew it automatically, or sign in again if renewal fails. Full login expiry is not exposed; renewed credentials will be copied to the other hosts.")
    };
    Some((WarningKey { account: state.account, renewed: state.renewed, deadline, refresh, expired }, text))
}

/// How each warning begins after the tool's name. Only this watcher words a
/// notice this way, so a warning it announced before a service handover is
/// still recognised as its own.
const RENEWAL_ENDS: &str = " login renewal expires in ";
const RENEWAL_ENDED: &str = " login can no longer renew.";
const ACCESS: &str = " access token ";

fn warning_of(tool: LoginTool, text: &str) -> bool {
    text.strip_prefix(tool.label())
        .is_some_and(|rest| [RENEWAL_ENDS, RENEWAL_ENDED, ACCESS].iter().any(|start| rest.starts_with(start)))
}

#[derive(Default)]
pub struct Watcher {
    next: Option<Instant>,
    last: [Option<WarningKey>; 2],
    /// The warning announced for the current `last`, word for word.
    warnings: [Option<String>; 2],
    errors: [Option<String>; 2],
    configuration_error: Option<String>,
    polled: bool,
    /// Announced notices that no longer hold, for the service to withdraw.
    stale: Vec<String>,
}

impl Watcher {
    pub fn configuration_error(&mut self, error: Option<String>) -> Option<String> {
        let changed = error != self.configuration_error;
        let before = std::mem::replace(&mut self.configuration_error, error);
        if changed {
            self.stale.extend(before);
            self.configuration_error.clone()
        } else {
            None
        }
    }

    /// Notices that are no longer current: a renewal answered the warning, or an error was resolved or
    /// reworded. Before handover discards announcement state, include listed warnings that no longer apply.
    pub fn stale(&mut self, listed: impl Iterator<Item = String>) -> Vec<String> {
        let mut stale = std::mem::take(&mut self.stale);
        if self.polled {
            stale.extend(listed.filter(|text| {
                TOOLS
                    .into_iter()
                    .zip(&self.warnings)
                    .any(|(tool, current)| warning_of(tool, text) && current.as_deref() != Some(text.as_str()))
            }));
        }
        stale
    }
    /// Polling is service-owned, so this also runs when there are no hosts.
    pub fn poll(&mut self, paths: &LoginPaths, settings: &Settings, now: Instant, wall_ms: i64) -> Vec<String> {
        if self.next.is_some_and(|at| now < at) {
            return Vec::new();
        }
        self.next = Some(now + Duration::from_secs(settings.interval_seconds));
        self.polled = true;
        let mut notes = Vec::new();
        for (index, tool) in TOOLS.into_iter().enumerate() {
            if !settings.tools.contains(&tool) {
                self.last[index] = None;
                self.stale.extend(self.warnings[index].take());
                self.stale.extend(self.errors[index].take());
                continue;
            }
            let result = super::mirrors::reconcile_at(paths, tool, settings, wall_ms)
                .and_then(|_| newest_login(paths, tool, settings, wall_ms));
            match result {
                Err(error) => {
                    let text = format!("{} login synchronization: {error:#}", tool.label());
                    if self.errors[index].as_ref() != Some(&text) {
                        notes.push(text.clone());
                        self.stale.extend(self.errors[index].replace(text));
                    }
                }
                Ok(login) => {
                    self.stale.extend(self.errors[index].take());
                    let warning = login.as_ref().and_then(|login| warning(tool, login, settings, wall_ms));
                    let key = warning.as_ref().map(|(key, _)| key.clone());
                    if key != self.last[index] {
                        let text = warning.map(|(_, text)| text);
                        notes.extend(text.clone());
                        if let Some(old) = std::mem::replace(&mut self.warnings[index], text) {
                            if self.warnings[index].as_ref() != Some(&old) {
                                self.stale.push(old);
                            }
                        }
                        self.last[index] = key;
                    }
                }
            }
        }
        notes
    }
}

fn newest_login(paths: &LoginPaths, tool: LoginTool, settings: &Settings, now: i64) -> Result<Option<Value>> {
    // Preserve explicit refresh-expired metadata for the warning if no renewable
    // source remains; synchronization itself never exports that credential.
    match super::mirrors::newest(paths, tool, now, settings)? {
        Some(login) => Ok(Some(login)),
        None => {
            let mut expired: Option<(LoginState, Value)> = None;
            for destination in super::mirrors::destinations(paths, settings) {
                let Some(login) = read_login(destination, tool)? else { continue };
                let state = state_of(tool, &login, now);
                if state.account.is_some() && expired.as_ref().is_none_or(|(older, _)| state.renewed > older.renewed) {
                    expired = Some((state, login));
                }
            }
            Ok(expired.map(|(_, login)| login))
        }
    }
}

pub fn health(paths: &LoginPaths, settings: &Settings, now: i64) -> Result<Value> {
    let mut tools = Vec::new();
    for &tool in &settings.tools {
        let login = newest_login(paths, tool, settings, now)?;
        let state = login.as_ref().map(|login| state_of(tool, login, now)).unwrap_or_default();
        let expiry = login.as_ref().map(|login| expiry_of(tool, login)).unwrap_or_default();
        let warning = login.as_ref().and_then(|login| warning(tool, login, settings, now)).map(|(_, text)| text);
        tools.push(serde_json::json!({"tool":tool,"signed_in":state.account.is_some(),
            "renewed_ms":state.renewed,"rejected":state.rejected,"access_expires_ms":expiry.access_expires_ms,
            "refresh_expires_ms":expiry.refresh_expires_ms,"full_login_expiry_known":expiry.refresh_expires_ms.is_some(),"warning":warning}));
    }
    Ok(
        serde_json::json!({"tools":tools,"credential_mirrors":settings.credential_mirrors.len(),"warn_before_seconds":settings.warn_before_seconds}),
    )
}
