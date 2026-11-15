//! `cepheus install`: copy this binary to `~/.local/bin`.

use std::fs;
use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::{Context, Result};

use crate::paths;
use crate::service::current_exe_clean;

/// The installed program's name before the rename to Cepheus. Sessions
/// started before it run `~/.local/bin/<this> hook claude`, and a service
/// started from there hands over to whatever program then sits at that path.
pub const LEGACY_PROGRAM: &str = "agent-deck"; // legacy-name

/// The installed program's name in `~/.local/bin`.
pub const PROGRAM: &str = "cepheus";

pub fn run() -> i32 {
    match install() {
        Ok(msg) => {
            println!("{msg}");
            0
        }
        Err(err) => {
            eprintln!("cepheus: {err:#}");
            1
        }
    }
}

fn install() -> Result<String> {
    anyhow::ensure!(
        env!("CEPHEUS_BUILD_PROFILE") == "release" && env!("CEPHEUS_BUILD_OPT_LEVEL") == "3" && !cfg!(debug_assertions),
        "installation requires an optimized cargo --release build; debug and test builds are never installable"
    );
    let exe = current_exe_clean();
    let dir = paths::home().join(".local/bin");
    fs::create_dir_all(&dir)?;
    let dest = dir.join(PROGRAM);
    let mut lines = Vec::new();
    // An automatically upgraded peer may expose the new name through a
    // reverse alias. Materialize it before linking the earlier name forward.
    if fs::symlink_metadata(&dest).is_ok_and(|m| m.file_type().is_file())
        && fs::canonicalize(&exe).ok() == fs::canonicalize(&dest).ok()
    {
        lines.push(format!("{} is already the installed binary", dest.display()));
    } else {
        let tmp = dir.join(".cepheus.install.tmp");
        fs::copy(&exe, &tmp).with_context(|| format!("copying {}", exe.display()))?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
        fs::rename(&tmp, &dest)?;
        lines.push(format!(
            "installed {}\nA running service keeps its sessions; new sessions use the new binary's hooks.",
            dest.display()
        ));
    }
    if link_legacy(&dir, &dest)? {
        lines.push(format!("{} now runs {}", dir.join(LEGACY_PROGRAM).display(), dest.display()));
    }
    Ok(lines.join("\n"))
}

/// Replace an install from before the rename in `dir` with a link to
/// `dest`, atomically, so the path never stops running a program. A missing
/// path, or one that is already a link, is left alone.
pub fn link_legacy(dir: &Path, dest: &Path) -> Result<bool> {
    let legacy = dir.join(LEGACY_PROGRAM);
    if legacy == dest {
        return Ok(false);
    }
    match fs::symlink_metadata(&legacy) {
        Ok(meta) if meta.file_type().is_file() => {}
        _ => return Ok(false),
    }
    let name = dest.file_name().context("the installed program's name")?;
    let tmp = dir.join(format!(".{LEGACY_PROGRAM}.link.{}.tmp", std::process::id()));
    let _ = fs::remove_file(&tmp);
    symlink(name, &tmp).with_context(|| format!("linking {}", legacy.display()))?;
    if let Err(err) = fs::rename(&tmp, &legacy) {
        let _ = fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("replacing {}", legacy.display()));
    }
    Ok(true)
}

/// Expose the renamed command after native updates replace a managed peer's
/// earlier binary in place. Never replace an existing entry or change the
/// running binary: its service and hooks still use the earlier path.
pub fn link_renamed_peer(program: &Path) -> Result<bool> {
    let dir = paths::home().join(".local/bin");
    let Ok(managed) = fs::canonicalize(&dir) else { return Ok(false) };
    if program.file_name() != Some(std::ffi::OsStr::new(LEGACY_PROGRAM))
        || program.parent().and_then(|path| fs::canonicalize(path).ok()).as_ref() != Some(&managed)
    {
        return Ok(false);
    }
    let meta = fs::symlink_metadata(program)?;
    // SAFETY: geteuid has no preconditions.
    if !meta.file_type().is_file() || meta.uid() != unsafe { libc::geteuid() } {
        return Ok(false);
    }
    match symlink(LEGACY_PROGRAM, dir.join(PROGRAM)) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(err) => Err(err).context("linking the renamed peer command"),
    }
}
