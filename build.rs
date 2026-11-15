//! Compile the declarative bundled-script registry; runtime paths come from configuration.
use std::{env, fs, path::PathBuf};
fn main() {
    println!("cargo:rustc-env=CEPHEUS_BUILD_PROFILE={}", env::var("PROFILE").unwrap());
    println!("cargo:rustc-env=CEPHEUS_BUILD_OPT_LEVEL={}", env::var("OPT_LEVEL").unwrap());
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let manifest = root.join("plugins/bundled-lua.txt");
    println!("cargo:rerun-if-changed={}", manifest.display());
    let mut source = String::from("pub const BUNDLED_SCRIPTS: &[(&str, &str, &str)] = &[\n");
    for line in fs::read_to_string(manifest).unwrap().lines().filter(|s| !s.trim().is_empty() && !s.starts_with('#')) {
        let fields: Vec<_> = line.split('|').collect();
        assert_eq!(fields.len(), 3, "invalid bundled Lua declaration");
        let path = root.join(fields[2]).canonicalize().unwrap();
        assert!(path.starts_with(&root), "bundled script outside package");
        println!("cargo:rerun-if-changed={}", path.display());
        source.push_str(&format!("({:?}, {:?}, include_str!({:?})),\n", fields[0], fields[1], path));
    }
    source.push_str("];\n");
    fs::write(PathBuf::from(env::var("OUT_DIR").unwrap()).join("bundled_plugins.rs"), source).unwrap();
}
