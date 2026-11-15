//! Local credential consumers share the newest login without sharing settings.
use super::*;

pub(super) fn destinations<'a>(paths: &'a LoginPaths, settings: &'a Settings) -> impl Iterator<Item = &'a LoginPaths> {
    std::iter::once(paths).chain(settings.credential_mirrors.iter().filter(move |p| *p != paths))
}

/// Reject conflicting active accounts before choosing or writing any consumer.
pub(super) fn newest(paths: &LoginPaths, tool: LoginTool, now: i64, settings: &Settings) -> Result<Option<Value>> {
    for mirror in &settings.credential_mirrors {
        let directory = match tool {
            LoginTool::Claude => &mirror.claude_dir,
            LoginTool::Codex => &mirror.codex_dir,
        };
        if !directory.is_dir() {
            bail!("configured credential mirror directory is unavailable");
        }
    }
    let mut account = None;
    let mut selected: Option<(LoginState, Value)> = None;
    for destination in destinations(paths, settings) {
        let Some(login) = read_login(destination, tool)? else { continue };
        let current = state_of(tool, &login, now);
        if current.account.is_none() || current.rejected {
            continue;
        }
        if let Some(expected) = &account {
            if current.account.as_ref() != Some(expected) {
                bail!("credential consumers are signed in to different active accounts; all are preserved");
            }
        } else {
            account = current.account.clone();
        }
        if selected.as_ref().is_none_or(|(state, _)| current.renewed > state.renewed) {
            selected = Some((current, login));
        }
    }
    Ok(selected.map(|(_, login)| login))
}

pub fn state_with_settings(paths: &LoginPaths, tool: LoginTool, now: i64, settings: &Settings) -> Result<LoginState> {
    Ok(match newest(paths, tool, now, settings)? {
        Some(login) => state_of(tool, &login, now),
        None => state(paths, tool, now),
    })
}

pub fn export_with_settings(paths: &LoginPaths, tool: LoginTool, settings: &Settings) -> Result<Value> {
    newest(paths, tool, now_ms(), settings)?.context("no renewable sign-in here")
}

/// A late network reply cannot overwrite a login renewed while it travelled.
pub fn import_with_settings(paths: &LoginPaths, tool: LoginTool, login: &Value, settings: &Settings) -> Result<()> {
    import_at(paths, tool, login, settings, now_ms())
}

fn import_at(paths: &LoginPaths, tool: LoginTool, login: &Value, settings: &Settings, now: i64) -> Result<()> {
    let incoming = state_of(tool, login, now);
    let Some(account) = incoming.account.as_ref() else { bail!("incoming sign-in is incomplete") };
    if incoming.rejected {
        bail!("incoming sign-in can no longer renew");
    }
    if let Some(current) = newest(paths, tool, now, settings)? {
        let current = state_of(tool, &current, now);
        if current.account.as_ref() != Some(account) {
            bail!("incoming sign-in belongs to another active account; credentials are preserved");
        }
        if current.renewed > incoming.renewed {
            return Ok(());
        }
    }
    for destination in destinations(paths, settings) {
        // Recheck each file, as a CLI can renew it between consumers.
        let current = read_login(destination, tool)?.map(|login| state_of(tool, &login, now)).unwrap_or_default();
        if current.account.is_some() && !current.rejected {
            if current.account.as_ref() != Some(account) {
                bail!("credential consumer changed account during synchronization; preserved");
            }
            if current.renewed >= incoming.renewed {
                continue;
            }
        }
        import(destination, tool, login)?;
    }
    Ok(())
}

pub(super) fn reconcile_at(paths: &LoginPaths, tool: LoginTool, settings: &Settings, now: i64) -> Result<()> {
    if let Some(login) = newest(paths, tool, now, settings)? {
        import_at(paths, tool, &login, settings, now)?;
    }
    Ok(())
}
