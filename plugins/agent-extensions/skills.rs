//! Bundle and manage agent skills.
//!
//! The deck keeps each skill in a folder of its own
//! (`<data>/skills/<name>`) and links it into every agent's skills folder:
//! `~/.claude/skills`, `~/.codex/skills` and `~/.agents/skills`, which
//! Antigravity reads. The skills it ships are written there from this
//! build; the installer adds others (`installer.rs`), recorded in
//! `<data>/skills/manifest.json` with the agents each is for. Only links
//! that point into the deck's folder are ever replaced or removed: a skill
//! of the owner's own with the same name is left alone and reported.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

/// A skill this build ships: its name and files.
pub struct Bundled {
    pub name: &'static str,
    pub files: &'static [(&'static str, &'static str)],
}

pub const BUNDLED: &[Bundled] = &[
    Bundled { name: "cepheus", files: &[("SKILL.md", include_str!("../../skills/cepheus/SKILL.md"))] },
    Bundled {
        name: "cepheus-plugin",
        files: &[
            ("SKILL.md", include_str!("../../skills/cepheus-plugin/SKILL.md")),
            ("api.md", include_str!("../../skills/cepheus-plugin/api.md")),
        ],
    },
];

const LEGACY_BUNDLED: &[(&str, &str)] = &[("agent-deck", "cepheus"), ("agent-deck-plugin", "cepheus-plugin")]; // legacy-name

/// Each agent's skills folder under the home directory, and the folder
/// whose presence says the agent is used here.
pub const AGENTS: &[(&str, &str, &str)] = &[
    ("claude", ".claude/skills", ".claude"),
    ("codex", ".codex/skills", ".codex"),
    ("agy", ".agents/skills", ".gemini"),
];

/// The manifest of installed skills.
pub const MANIFEST: &str = "manifest.json";
/// The most files and bytes an installed skill may have.
pub const MAX_FILES: usize = 400;
pub const MAX_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Installed {
    /// Where it came from, as the owner gave it.
    pub source: String,
    /// The agents it is linked for; empty: every one.
    #[serde(default)]
    pub agents: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub installed: BTreeMap<String, Installed>,
}

/// What a sync did.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SyncReport {
    pub written: Vec<String>,
    pub linked: Vec<String>,
    pub removed: Vec<String>,
    pub preserved: Vec<String>,
    pub problems: Vec<String>,
}

impl SyncReport {
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if !self.written.is_empty() {
            parts.push(format!("updated {}", self.written.join(", ")));
        }
        if !self.linked.is_empty() {
            parts.push(format!("linked {}", self.linked.join(", ")));
        }
        if !self.removed.is_empty() {
            parts.push(format!("unlinked {}", self.removed.join(", ")));
        }
        parts.extend(self.preserved.iter().cloned());
        parts.extend(self.problems.iter().cloned());
        if parts.is_empty() {
            "The deck's skills are linked for every agent".to_string()
        } else {
            format!("Skills: {}", parts.join("; "))
        }
    }
}

/// A skill name as the deck takes it: a folder name of ordinary characters.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub fn store(data: &Path) -> PathBuf {
    data.join("skills")
}

pub fn read_manifest(store: &Path) -> Manifest {
    let path = store.join(MANIFEST);
    match fs::metadata(&path) {
        Ok(m) if m.len() <= 1024 * 1024 => {
            fs::read(&path).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default()
        }
        _ => Manifest::default(),
    }
}

pub fn write_manifest(store: &Path, manifest: &Manifest) -> Result<()> {
    let data = serde_json::to_vec_pretty(manifest)?;
    crate::util::write_atomic(&store.join(MANIFEST), &data).context("saving the skills manifest")
}

/// Write the skills this build ships into the store, where they differ.
pub fn write_bundled(store: &Path) -> Result<Vec<String>> {
    let mut written = Vec::new();
    for skill in BUNDLED {
        let dir = store.join(skill.name);
        fs::create_dir_all(&dir).with_context(|| format!("making {}", dir.display()))?;
        let mut changed = false;
        for (file, text) in skill.files {
            let path = dir.join(file);
            if fs::read(&path).ok().as_deref() != Some(text.as_bytes()) {
                crate::util::write_atomic(&path, text.as_bytes())
                    .with_context(|| format!("writing {}", path.display()))?;
                changed = true;
            }
        }
        if changed {
            written.push(skill.name.to_string());
        }
    }
    Ok(written)
}

/// Whether `link` is a link the deck made: it points into `store`.
fn deck_link(link: &Path, store: &Path) -> Option<PathBuf> {
    let meta = fs::symlink_metadata(link).ok()?;
    if !meta.file_type().is_symlink() {
        return None;
    }
    let target = fs::read_link(link).ok()?;
    target.starts_with(store).then_some(target)
}

/// The agents a skill is for: every one when `agents` is empty.
fn wants(agents: &[String], agent: &str) -> bool {
    agents.is_empty() || agents.iter().any(|a| a == agent)
}

/// Link every skill in the store into each agent's skills folder it is for,
/// and remove the deck's links for skills it no longer has.
pub fn link_all(store: &Path, home: &Path, report: &mut SyncReport) {
    let manifest = read_manifest(store);
    // Only skills the store holds: a link to one not written yet would lead
    // nowhere.
    let mut skills: BTreeMap<String, Vec<String>> =
        BUNDLED.iter().filter(|b| store.join(b.name).is_dir()).map(|b| (b.name.to_string(), Vec::new())).collect();
    for (name, installed) in &manifest.installed {
        if valid_name(name) && store.join(name).is_dir() {
            skills.insert(name.clone(), installed.agents.clone());
        }
    }
    for &(agent, rel, marker) in AGENTS {
        if !home.join(marker).is_dir() {
            continue;
        }
        let dir = home.join(rel);
        if let Err(err) = fs::create_dir_all(&dir) {
            report.problems.push(format!("could not make {}: {err}", dir.display()));
            continue;
        }
        for (name, agents) in &skills {
            let link = dir.join(name);
            let target = store.join(name);
            let want = wants(agents, agent);
            match (fs::symlink_metadata(&link), deck_link(&link, store)) {
                (Err(e), _) if e.kind() == io::ErrorKind::NotFound => {
                    if want {
                        match symlink(&target, &link) {
                            Ok(()) => report.linked.push(format!("{name} for {agent}")),
                            Err(err) => report.problems.push(format!("could not link {}: {err}", link.display())),
                        }
                    }
                }
                (Ok(_), Some(current)) => {
                    if !want {
                        if fs::remove_file(&link).is_ok() {
                            report.removed.push(format!("{name} for {agent}"));
                        }
                    } else if current != target {
                        let _ = fs::remove_file(&link);
                        if symlink(&target, &link).is_ok() {
                            report.linked.push(format!("{name} for {agent}"));
                        }
                    }
                }
                (Ok(_), None) => {
                    if want {
                        report
                            .preserved
                            .push(format!("kept your own {} (the deck's {name} is not linked)", link.display()));
                    }
                }
                (Err(err), _) => report.problems.push(format!("could not read {}: {err}", link.display())),
            }
        }
        // The deck's links to skills it no longer has.
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let link = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if skills.contains_key(&name) || LEGACY_BUNDLED.iter().any(|(old, _)| *old == name) {
                continue;
            }
            if deck_link(&link, store).is_some() && fs::remove_file(&link).is_ok() {
                report.removed.push(format!("{name} for {agent}"));
            }
        }
    }
    if let Some(data) = store.parent() {
        retire_legacy_links(data, home, report);
    }
}

/// Hold the lock on the deck's install records in `data` (this manifest
/// and the links made from it, the installer's `installed.json` and the
/// deck's marketplace) until the file returned is dropped. The service runs
/// two installs at once and `cepheus add` can run beside it, so each change
/// reads the records afresh under this lock instead of publishing a copy
/// read earlier, which dropped another install's entries (audit
/// OPS-S11601). Holders never take it twice.
pub fn lock(data: &Path) -> Result<fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    crate::util::ensure_private_dir(data).with_context(|| format!("making {}", data.display()))?;
    let path = data.join(".install.lock");
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_NOFOLLOW)
        .mode(0o600)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    // SAFETY: flock operates on a descriptor this call owns; dropping it unlocks.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(io::Error::last_os_error()).with_context(|| format!("locking {}", path.display()));
    }
    Ok(file)
}

/// Write the shipped skills and link every skill for its agents.
pub fn sync(data: &Path, home: &Path) -> Result<SyncReport> {
    let store = store(data);
    let _lock = lock(data)?;
    fs::create_dir_all(&store).with_context(|| format!("making {}", store.display()))?;
    let mut report = SyncReport { written: write_bundled(&store)?, ..SyncReport::default() };
    link_all(&store, home, &mut report);
    Ok(report)
}

/// Retire only the old bundled discovery links after their replacements are
/// installed. Keep legacy contents recoverable and never adopt a whole old
/// store as ours: it can also hold the owner's installed skills.
fn retire_legacy_links(data: &Path, home: &Path, report: &mut SyncReport) {
    let store = store(data);
    let installed = read_manifest(&store).installed;
    let mut old_stores = vec![store.clone(), home.join(".local/share/agent-deck/skills")]; // legacy-name
    if data.file_name().is_some_and(|name| name == "cepheus") {
        old_stores.push(data.with_file_name("agent-deck").join("skills")); // legacy-name
    }
    for &(agent, rel, marker) in AGENTS {
        if !home.join(marker).is_dir() {
            continue;
        }
        let dir = home.join(rel);
        for &(old, current) in LEGACY_BUNDLED {
            if installed.contains_key(old)
                || !store.join(current).join("SKILL.md").is_file()
                || fs::read_link(dir.join(current)).ok().as_ref() != Some(&store.join(current))
            {
                continue;
            }
            let link = dir.join(old);
            let Ok(target) = fs::read_link(&link) else { continue };
            let Some(root) = old_stores.iter().find(|root| target == root.join(old)) else { continue };
            // A separate legacy store can have its own explicit installations.
            // Missing records mean none; unreadable or invalid records prove
            // nothing, so preserve the link rather than guessing ownership.
            let manifest_path = root.join(MANIFEST);
            let legacy_manifest = match fs::symlink_metadata(&manifest_path) {
                Err(err) if err.kind() == io::ErrorKind::NotFound => Some(Manifest::default()),
                Ok(meta) if meta.is_file() && meta.len() <= 1024 * 1024 => {
                    fs::read(&manifest_path).ok().and_then(|bytes| serde_json::from_slice::<Manifest>(&bytes).ok())
                }
                _ => None,
            };
            if legacy_manifest.is_none_or(|manifest| manifest.installed.contains_key(old)) {
                report.preserved.push(format!("kept {old} for {agent}: legacy installation ownership is not ours"));
                continue;
            }
            match fs::remove_file(&link) {
                Ok(()) => report.removed.push(format!("{old} for {agent}")),
                Err(err) => report.problems.push(format!("could not unlink {}: {err}", link.display())),
            }
        }
    }
}

/// Copy a skill folder from a fetched source into the store as `name`
/// (replacing an earlier install of it), record it and link it. Links in
/// the source are not followed and nothing outside it is copied.
pub fn install_dir(
    data: &Path,
    home: &Path,
    src: &Path,
    name: &str,
    agents: &[String],
    source: &str,
) -> Result<SyncReport> {
    if !valid_name(name) {
        bail!("{name:?} is not a skill name the deck takes");
    }
    if BUNDLED.iter().any(|b| b.name == name) {
        bail!("{name} is one of the deck's own skills");
    }
    if !src.join("SKILL.md").is_file() {
        bail!("{} has no SKILL.md", src.display());
    }
    let store = store(data);
    fs::create_dir_all(&store)?;
    let staging = store.join(format!(".{name}.new"));
    let _ = fs::remove_dir_all(&staging);
    let mut budget = (MAX_FILES, MAX_BYTES);
    if let Err(err) = copy_tree(src, &staging, &mut budget) {
        let _ = fs::remove_dir_all(&staging);
        return Err(err);
    }
    let dest = store.join(name);
    // Another install's `link_all` must not see this folder without its
    // manifest entry, nor an earlier manifest, and prune links by it.
    let _lock = lock(data)?;
    if dest.exists() {
        fs::remove_dir_all(&dest).with_context(|| format!("replacing {}", dest.display()))?;
    }
    fs::rename(&staging, &dest)?;
    let mut manifest = read_manifest(&store);
    manifest.installed.insert(name.to_string(), Installed { source: source.to_string(), agents: agents.to_vec() });
    write_manifest(&store, &manifest)?;
    let mut report = SyncReport::default();
    link_all(&store, home, &mut report);
    Ok(report)
}

/// Remove an installed skill: its links, its folder and its record.
pub fn remove(data: &Path, home: &Path, name: &str) -> Result<SyncReport> {
    if !valid_name(name) || BUNDLED.iter().any(|b| b.name == name) {
        bail!("{name} is not a skill the deck installed");
    }
    let store = store(data);
    let _lock = lock(data)?;
    let mut manifest = read_manifest(&store);
    if manifest.installed.remove(name).is_none() {
        bail!("the deck did not install a skill named {name}");
    }
    // The folder goes before its record, so a removal that fails can be
    // tried again (as audit ROOT-037 found of the installer's receipts).
    let dir = store.join(name);
    if dir.is_dir() {
        fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    write_manifest(&store, &manifest)?;
    let mut report = SyncReport::default();
    // Ordinary sync preserves old bundled aliases until replacements exist;
    // an explicit uninstall still removes this skill's exact managed links.
    if LEGACY_BUNDLED.iter().any(|(old, _)| *old == name) {
        for &(agent, rel, _) in AGENTS {
            let link = home.join(rel).join(name);
            if fs::read_link(&link).ok().as_ref() == Some(&store.join(name)) {
                match fs::remove_file(&link) {
                    Ok(()) => report.removed.push(format!("{name} for {agent}")),
                    Err(err) => report.problems.push(format!("could not unlink {}: {err}", link.display())),
                }
            }
        }
    }
    link_all(&store, home, &mut report);
    Ok(report)
}

fn copy_tree(src: &Path, dest: &Path, budget: &mut (usize, u64)) -> Result<()> {
    fs::create_dir_all(dest)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let kind = entry.file_type()?;
        let from = entry.path();
        let to = dest.join(&name);
        if kind.is_dir() {
            copy_tree(&from, &to, budget)?;
        } else if kind.is_file() {
            let len = entry.metadata()?.len();
            if budget.0 == 0 || budget.1 < len {
                bail!("the skill is larger than the deck copies ({MAX_FILES} files, {MAX_BYTES} bytes)");
            }
            budget.0 -= 1;
            budget.1 -= len;
            fs::copy(&from, &to)?;
        }
        // Links and anything else are left out.
    }
    Ok(())
}

/// A skill's one-line description, from its `SKILL.md` front matter.
pub fn description(skill_md: &str) -> Option<String> {
    let rest = skill_md.strip_prefix("---")?;
    let (front, _) = rest.split_once("\n---")?;
    front.lines().find_map(|line| {
        line.strip_prefix("description:").map(|d| d.trim().trim_matches('"').to_string()).filter(|d| !d.is_empty())
    })
}

/// A skill and where it is linked, for `cepheus skills` and `:skills`.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub name: String,
    pub description: String,
    pub source: String,
    pub linked: Vec<String>,
}

pub fn list(data: &Path, home: &Path) -> Vec<Status> {
    let store = store(data);
    let manifest = read_manifest(&store);
    let names: Vec<(String, String)> = BUNDLED
        .iter()
        .map(|b| (b.name.to_string(), "cepheus".to_string()))
        .chain(manifest.installed.iter().map(|(n, i)| (n.clone(), i.source.clone())))
        .collect();
    names
        .into_iter()
        .map(|(name, source)| {
            let text = fs::read_to_string(store.join(&name).join("SKILL.md")).unwrap_or_default();
            let linked = AGENTS
                .iter()
                .filter(|(_, rel, _)| deck_link(&home.join(rel).join(&name), &store).is_some())
                .map(|(agent, _, _)| agent.to_string())
                .collect();
            Status { description: description(&text).unwrap_or_default(), name, source, linked }
        })
        .collect()
}

/// `cepheus skills [list|sync|remove <name>]`.
pub fn run(args: &[String]) -> i32 {
    let data = crate::paths::data_dir();
    let home = crate::paths::home();
    let result: Result<()> = match args.first().map(String::as_str) {
        None | Some("list") => {
            for s in list(&data, &home) {
                let linked = if s.linked.is_empty() { "not linked".to_string() } else { s.linked.join(", ") };
                println!("{:<24} {:<12} {}\n{:<24} {}", s.name, linked, s.source, "", s.description);
            }
            Ok(())
        }
        Some("sync") => sync(&data, &home).map(|r| println!("{}", r.summary())),
        Some("remove") => match args.get(1) {
            Some(name) => remove(&data, &home, name).map(|r| println!("Removed {name}. {}", r.summary())),
            None => Err(anyhow::anyhow!("usage: cepheus skills remove <name>")),
        },
        Some(other) => Err(anyhow::anyhow!("unknown skills command {other:?} (list, sync, remove)")),
    };
    match result {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("cepheus: {err:#}");
            1
        }
    }
}

#[cfg(test)]
#[path = "../../src/tests/skills.rs"]
mod tests;
