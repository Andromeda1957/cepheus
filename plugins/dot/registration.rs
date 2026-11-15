//! Validate and move only the bridge's own provider registration.
//!
//! Reading is pure: it opens no state for writing, creates nothing and runs
//! nothing. Moving a registration to this deck holds the controller lock in
//! the pinned state directory and changes only the provider's command, so the
//! settings, pinned room, delivery journal and feed stay exactly as they were.
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};

use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{json, Map, Value};

use super::state::{absolute, write_bytes, FILE_MAX};
use super::Error;

const KEYS: [&str; 8] =
    ["id", "label", "command", "broadcast", "cloud", "chat_snapshot", "working_patterns", "prompt_patterns"];
const STATE_FILES: [&str; 4] = ["settings.json", "selection.json", "pending.json", "transcript.json"];
const CUSTOM: &str = "A custom Dot provider is configured; it was preserved.";
const UNTRUSTED: &str = "The Dot helper or state snapshot is not a trusted managed registration.";
const CHANGED: &str = "Dot metadata changed during validation; retry without changing the registration.";

pub struct Record {
    pub path: PathBuf,
    data: Vec<u8>,
    provider: Map<String, Value>,
    pub state: PathBuf,
    /// The deck program in `<deck> dot chat --state-dir DIR`.
    program: PathBuf,
    private: Vec<(&'static str, Option<Vec<u8>>)>,
    identity: (u64, u64),
    profile: Option<Vec<u8>>,
    config: PathBuf,
}

impl Record {
    /// Whether the registration already runs `program`.
    pub fn runs(&self, program: &Path) -> bool {
        self.program == program
    }
}

/// Resolve links one component at a time, rejecting process/device paths
/// before resolution can hide them. Never execute registration input.
pub fn stable_program(path: &Path) -> Result<PathBuf, Error> {
    let mut current = PathBuf::from("/");
    let mut remaining: std::collections::VecDeque<_> =
        path.components().skip(1).map(|c| c.as_os_str().to_owned()).collect();
    let mut links = 0;
    for _ in 0..4096 {
        let Some(word) = remaining.pop_front() else { return Ok(current) };
        if word == "." || word.is_empty() {
            continue;
        }
        if word == ".." {
            current.pop();
            continue;
        }
        if current == Path::new("/") && ["proc", "sys", "dev"].iter().any(|name| word == *name) {
            return Err(Error::Invalid("The registered program must have a stable trusted path.".into()));
        }
        let candidate = current.join(&word);
        if fs::symlink_metadata(&candidate).is_ok_and(|m| m.file_type().is_symlink()) {
            links += 1;
            if links > 40 {
                return Err(Error::Invalid("The registered program has an invalid link chain.".into()));
            }
            let target = fs::read_link(candidate)?;
            if target.is_absolute() {
                current = PathBuf::from("/");
            }
            let mut components: std::collections::VecDeque<_> = target
                .components()
                .filter(|c| !matches!(c, Component::RootDir))
                .map(|c| c.as_os_str().to_owned())
                .collect();
            components.append(&mut remaining);
            remaining = components;
        } else {
            current = candidate;
        }
    }
    Err(Error::Invalid("The registered program has an invalid path.".into()))
}

fn same_file(a: &Path, b: &Path) -> bool {
    fs::metadata(a)
        .ok()
        .zip(fs::metadata(b).ok())
        .is_some_and(|(a, b)| a.is_file() && b.is_file() && (a.dev(), a.ino()) == (b.dev(), b.ino()))
}

/// The provider manifest for `program`, in the one shape this module trusts.
pub fn provider(program: &Path, state: &Path) -> Value {
    json!({
        "id": "dot",
        "label": "Dot",
        "command": [program.to_string_lossy(), "dot", "chat", "--state-dir", state.to_string_lossy()],
        "broadcast": true,
        "cloud": true,
        "chat_snapshot": state.join("transcript.json").to_string_lossy(),
        "working_patterns": ["Dot is working"],
        "prompt_patterns": [],
    })
}

/// JSON that refuses a repeated key anywhere.
struct Strict(Value);

impl<'de> Deserialize<'de> for Strict {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Value;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("JSON")
            }
            fn visit_unit<E>(self) -> Result<Value, E> {
                Ok(Value::Null)
            }
            fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
                Ok(Value::Bool(v))
            }
            fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
                Ok(v.into())
            }
            fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
                Ok(v.into())
            }
            fn visit_f64<E>(self, v: f64) -> Result<Value, E> {
                Ok(serde_json::Number::from_f64(v).map_or(Value::Null, Value::Number))
            }
            fn visit_str<E>(self, v: &str) -> Result<Value, E> {
                Ok(Value::String(v.to_string()))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
                let mut items = Vec::new();
                while let Some(Strict(item)) = seq.next_element()? {
                    items.push(item);
                }
                Ok(Value::Array(items))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
                let mut out = Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    let Strict(value) = map.next_value()?;
                    if out.insert(key, value).is_some() {
                        return Err(de::Error::custom("duplicate key"));
                    }
                }
                Ok(Value::Object(out))
            }
        }
        deserializer.deserialize_any(V).map(Strict)
    }
}

pub fn decode(data: &[u8]) -> Result<Value, Error> {
    serde_json::from_slice::<Strict>(data).map(|Strict(value)| value).map_err(|err| {
        Error::Invalid(
            if err.to_string().contains("duplicate key") {
                "Duplicate key in Dot metadata."
            } else {
                "Invalid Dot metadata file."
            }
            .into(),
        )
    })
}

/// A regular file's bytes, `None` when it does not exist. Links and special
/// files are refused without blocking.
pub fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>, Error> {
    let file = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > FILE_MAX {
        return Err(Error::Invalid("Invalid Dot metadata file.".into()));
    }
    let mut data = Vec::new();
    file.take(FILE_MAX + 1).read_to_end(&mut data)?;
    Ok(Some(data))
}

/// An absolute path written exactly as its components.
fn absolute_path(value: &Value) -> Result<PathBuf, Error> {
    let text = match value.as_str() {
        Some(text) if !text.is_empty() && text.chars().count() <= 4096 && !text.chars().any(|c| (c as u32) < 32) => {
            text
        }
        _ => return Err(Error::Invalid("Invalid managed Dot path.".into())),
    };
    let path = PathBuf::from(text);
    let rebuilt: PathBuf = path.components().collect();
    if !path.is_absolute()
        || rebuilt.as_os_str().as_bytes() != text.as_bytes()
        || path.components().any(|c| c == Component::ParentDir)
    {
        return Err(Error::Invalid("Managed Dot paths must be absolute and canonical.".into()));
    }
    Ok(path)
}

/// The registered deck program and state directory, without opening any file.
fn validate_provider(value: &Value) -> Result<(PathBuf, PathBuf), Error> {
    let custom = || Error::Invalid(CUSTOM.into());
    let Value::Object(map) = value else { return Err(custom()) };
    if map.len() != KEYS.len() || KEYS.iter().any(|key| !map.contains_key(*key)) {
        return Err(custom());
    }
    let command: Vec<&str> = match map["command"].as_array() {
        Some(words) if words.len() == 5 && words.iter().all(Value::is_string) => {
            words.iter().filter_map(Value::as_str).collect()
        }
        _ => return Err(custom()),
    };
    if map["id"] != "dot"
        || map["label"] != "Dot"
        || map["broadcast"] != Value::Bool(true)
        || map["cloud"] != Value::Bool(true)
        || map["working_patterns"] != json!(["Dot is working"])
        || map["prompt_patterns"] != json!([])
        || command[1..4] != ["dot", "chat", "--state-dir"]
    {
        return Err(custom());
    }
    let program = absolute_path(&map["command"][0])?;
    let directory = absolute_path(&map["command"][4])?;
    if map["chat_snapshot"].as_str() != Some(directory.join("transcript.json").to_string_lossy().as_ref()) {
        return Err(Error::Invalid(UNTRUSTED.into()));
    }
    Ok((program, directory))
}

/// The managed registration in `config`, if there is one. `explicit_state`
/// must be its state directory; `quiescent` also refuses any delivery journal.
pub fn read(config: &Path, explicit_state: Option<&Path>, quiescent: bool) -> Result<Option<Record>, Error> {
    let path = config.join("providers/dot.json");
    let Some(data) = read_bytes(&path)? else { return Ok(None) };
    let value = decode(&data)?;
    let (program, directory) = validate_provider(&value)?;
    let stable = stable_program(&program)?;
    let trusted = std::env::current_exe()?;
    if !same_file(&stable, &trusted) || !same_file(&program, &trusted) {
        return Err(Error::Invalid(
            "Run Dot setup with the registered trusted program; metadata was preserved.".into(),
        ));
    }
    if let Some(explicit) = explicit_state {
        let explicit = absolute_path(&Value::String(absolute(explicit).to_string_lossy().into_owned()))?;
        if explicit != directory {
            return Err(Error::Invalid(
                "The explicit Dot state differs from its registration; metadata was preserved.".into(),
            ));
        }
    }
    if directory.ancestors().any(|p| fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink())) {
        return Err(Error::Invalid("Dot state must not use symlinked directories.".into()));
    }
    let info = fs::metadata(&directory)?;
    // SAFETY: getuid has no preconditions.
    if !info.is_dir() || info.uid() != unsafe { libc::getuid() } || info.mode() & 0o077 != 0 {
        return Err(Error::Invalid("The registered private Dot state is unavailable or insecure.".into()));
    }
    let mut private = Vec::new();
    for name in STATE_FILES {
        private.push((name, read_bytes(&directory.join(name))?));
    }
    if quiescent {
        if let Some((_, Some(pending))) = private.iter().find(|(name, _)| *name == "pending.json") {
            if !decode(pending)?.is_null() {
                return Err(Error::Invalid(
                    "Check uncertain delivery and /resolve before relocating Dot metadata.".into(),
                ));
            }
        }
    }
    let Value::Object(provider) = value else { return Err(Error::Invalid(CUSTOM.into())) };
    Ok(Some(Record {
        path,
        data,
        provider,
        state: directory,
        program,
        private,
        identity: (info.dev(), info.ino()),
        profile: read_bytes(&config.join("profile.json"))?,
        config: config.to_path_buf(),
    }))
}

fn unchanged(record: &Record) -> Result<(), Error> {
    let info = fs::metadata(&record.state)?;
    if (info.dev(), info.ino()) != record.identity
        || read_bytes(&record.path)?.as_deref() != Some(record.data.as_slice())
        || read_bytes(&record.config.join("profile.json"))? != record.profile
    {
        return Err(Error::Invalid(CHANGED.into()));
    }
    for (name, data) in &record.private {
        if &read_bytes(&record.state.join(name))? != data {
            return Err(Error::Invalid(CHANGED.into()));
        }
    }
    Ok(())
}

/// A quiescent registration with its controller lock held in the pinned
/// state directory, so no controller can start while it changes.
pub struct Held {
    pub record: Record,
    _directory: File,
    _lock: Option<File>,
}

pub fn hold(config: &Path, explicit_state: Option<&Path>) -> Result<Option<Held>, Error> {
    hold_mode(config, explicit_state, true)
}

fn hold_mode(config: &Path, explicit_state: Option<&Path>, create: bool) -> Result<Option<Held>, Error> {
    let Some(record) = read(config, explicit_state, true)? else { return Ok(None) };
    let directory =
        OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(&record.state)?;
    let pinned = directory.metadata()?;
    if (pinned.dev(), pinned.ino()) != record.identity {
        return Err(Error::Invalid("The registered Dot state directory changed; retry.".into()));
    }
    // SAFETY: openat relative to the pinned directory descriptor, with a
    // constant NUL-terminated name; the result is owned below.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c"bridge.lock".as_ptr(),
            (if create { libc::O_RDWR | libc::O_CREAT } else { libc::O_RDONLY })
                | libc::O_NOFOLLOW
                | libc::O_NONBLOCK
                | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if descriptor < 0 {
        if !create && io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
            unchanged(&record)?;
            return Ok(Some(Held { record, _directory: directory, _lock: None }));
        }
        return Err(io::Error::last_os_error().into());
    }
    // SAFETY: a new descriptor nothing else owns.
    let lock = unsafe { File::from_raw_fd(descriptor) };
    if !lock.metadata()?.is_file() {
        return Err(Error::Invalid("Invalid Dot controller lock.".into()));
    }
    // SAFETY: flock on the descriptor owned by `lock`.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            return Err(Error::Invalid(
                "Dot has an active controller; disconnect it before relocating metadata.".into(),
            ));
        }
        return Err(err.into());
    }
    unchanged(&record)?;
    Ok(Some(Held { record, _directory: directory, _lock: Some(lock) }))
}

/// Point the registration at `program`'s `dot chat`. Returns whether it changed.
pub fn relocate(held: &Held, program: &Path) -> Result<bool, Error> {
    let record = &held.record;
    let program = absolute_path(&Value::String(program.to_string_lossy().into_owned()))?;
    if !program.is_file() {
        return Err(Error::Invalid("The deck program is unavailable; the registration was preserved.".into()));
    }
    unchanged(record)?;
    if record.runs(&program) {
        return Ok(false);
    }
    let mut provider = record.provider.clone();
    provider.insert("command".into(), provider_command(&program, &record.state));
    let mut data = serde_json::to_vec_pretty(&Value::Object(provider)).map_err(io::Error::other)?;
    data.push(b'\n');
    write_bytes(&record.path, &data)?;
    Ok(true)
}

/// Publish repository plugin selection for an already native registration.
/// The existing installer delegates here so validation and publication share
/// the same pinned controller lock. Dry-run never creates a lock or state.
pub fn source_mode(config: &Path, source: &Path, dry_run: bool) -> Result<(), Error> {
    let source = source.canonicalize()?;
    if !source.join("plugins/catalog.json").is_file() || !source.join("plugins/dot.lua").is_file() {
        return Err(Error::Invalid("The selected repository plugin source is unavailable.".into()));
    }
    let record = read(config, None, true)?.ok_or_else(|| Error::Invalid("The Dot provider is unavailable.".into()))?;
    let mut profile = match &record.profile {
        Some(data) => decode(data)?,
        None => Value::Object(Map::new()),
    };
    if !profile.is_object() {
        return Err(Error::Invalid("The local profile must be an object.".into()));
    }
    profile["plugin_source_dir"] = source.to_string_lossy().into_owned().into();
    let held = hold_mode(config, None, !dry_run)?.ok_or_else(|| Error::Invalid(CHANGED.into()))?;
    if held.record.data != record.data || held.record.profile != record.profile {
        return Err(Error::Invalid(CHANGED.into()));
    }
    unchanged(&held.record)?;
    if dry_run {
        return Ok(());
    }
    if let Some(original) = &record.profile {
        let backup = config.join(format!("profile.json.before-plugin-update-{}", crate::util::uuid_v4()?));
        write_bytes(&backup, original)?;
    }
    super::state::write_json(&config.join("profile.json"), &profile)
}

fn provider_command(program: &Path, state: &Path) -> Value {
    provider(program, state)["command"].clone()
}
