//! Resolve catalogued Lua entry points without falling back to installed copies.
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

fn read(path: &Path) -> Result<String, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut text = String::new();
    file.take(1024 * 1024 + 1).read_to_string(&mut text).map_err(|e| e.to_string())?;
    if text.len() > 1024 * 1024 {
        return Err(format!("{} exceeds 1 MiB", path.display()));
    }
    Ok(text)
}

fn resolve(root: &Path, relative: &str) -> Result<PathBuf, String> {
    if relative.is_empty() || Path::new(relative).components().any(|c| !matches!(c, Component::Normal(_))) {
        return Err(format!("invalid plugin source: {relative}"));
    }
    let path = root.join(relative).canonicalize().map_err(|e| format!("{relative}: {e}"))?;
    if !path.starts_with(root) || !path.is_file() {
        return Err(format!("plugin source outside source directory: {relative}"));
    }
    Ok(path)
}

pub fn files(directory: &Path, service: bool) -> Result<Vec<(String, PathBuf)>, String> {
    let root = directory.canonicalize().map_err(|e| format!("{}: {e}", directory.display()))?;
    let mut files = BTreeMap::new();
    for catalog in ["plugins/catalog.json", "private/catalog.json"] {
        if catalog.starts_with("private/") && !root.join(catalog).exists() {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(&read(&resolve(&root, catalog)?)?).map_err(|e| e.to_string())?;
        let plugins = value
            .get("plugins")
            .and_then(|p| p.as_object())
            .filter(|_| value["version"] == 1)
            .ok_or("invalid plugin source catalog")?;
        for (id, plugin) in plugins {
            if !matches!(plugin["kind"].as_str(), Some("optional" | "private")) {
                continue;
            }
            for file in plugin["files"].as_array().ok_or("missing plugin files")? {
                let target = file["target"].as_str().ok_or("missing plugin target")?;
                let expected = format!("{}/{}.lua", if service { "service" } else { "plugins" }, id);
                if target == expected {
                    let source = file["source"].as_str().ok_or("missing plugin source")?;
                    if files.insert(id.clone(), resolve(&root, source)?).is_some() {
                        return Err(format!("duplicate plugin source: {id}"));
                    }
                }
            }
        }
    }
    if files.len() > 64 {
        return Err("more than 64 plugin source files".into());
    }
    Ok(files.into_iter().collect())
}

pub fn bundled(directory: &Path, id: &str, context: &str) -> Result<PathBuf, String> {
    let root = directory.canonicalize().map_err(|e| e.to_string())?;
    for line in read(&resolve(&root, "plugins/bundled-lua.txt")?)?.lines() {
        let fields: Vec<_> = line.split('|').collect();
        if fields.len() == 3 && fields[0] == id && fields[1] == context {
            return resolve(&root, fields[2]);
        }
    }
    Err(format!("missing bundled source: {id} ({context})"))
}
