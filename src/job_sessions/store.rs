//! Owner-private files; publication fsyncs the file and containing directory.
use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};
pub fn check_directory(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    anyhow::ensure!(
        meta.is_dir()
            && !meta.file_type().is_symlink()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.mode() & 0o077 == 0,
        "invalid private job directory"
    );
    Ok(())
}
pub fn directory(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
        if let Some(parent) = path.parent() {
            File::open(parent)?.sync_all()?;
        }
    }
    check_directory(path)
}
fn checked(file: File) -> Result<File> {
    let meta = file.metadata()?;
    anyhow::ensure!(
        meta.is_file() && meta.uid() == unsafe { libc::geteuid() } && meta.mode() & 0o077 == 0 && meta.nlink() == 1,
        "invalid private job file"
    );
    Ok(file)
}
pub fn read<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let mut file = checked(OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)?)?;
    anyhow::ensure!(file.metadata()?.len() <= 524288, "oversized job receipt");
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(serde_json::from_slice(&bytes)?)
}
pub fn optional<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::symlink_metadata(path) {
        Ok(_) => read(path).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
pub fn write<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("job record parent missing")?;
    check_directory(parent)?;
    let temporary = parent.join(format!(".{}.tmp", crate::util::uuid_v4()?));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&temporary)?;
    let result = (|| -> Result<()> {
        file.write_all(&serde_json::to_vec(value)?)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}
pub fn append_log(path: &Path) -> Result<File> {
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => Ok(file),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            checked(OpenOptions::new().append(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)?)
        }
        Err(e) => Err(e.into()),
    }
}
pub fn open_log(path: &Path) -> Result<File> {
    checked(OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)?)
}

/// Publish a complete launch intent atomically. A daemon dying before rename
/// leaves only an unaccepted staging directory, never a runnable half-record.
pub fn publish_intent<T: Serialize>(root: &Path, target: &Path, value: &T) -> Result<()> {
    anyhow::ensure!(!target.try_exists()?, "job receipt path already exists");
    let temporary = root.join(format!(".new-{}", crate::util::uuid_v4()?));
    directory(&temporary)?;
    write(&temporary.join("intent.json"), value)?;
    fs::rename(&temporary, target)?;
    File::open(root)?.sync_all()?;
    Ok(())
}
