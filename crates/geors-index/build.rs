//! Embed every synonym rule file from `<workspace>/synonyms/*.toml`, so new
//! languages need no Rust changes.

use std::path::PathBuf;

fn main() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../synonyms");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
        .collect();
    files.sort();
    let mut code = String::from(
        "/// (language, rule file contents), from `synonyms/*.toml`.\npub const BUILTIN: &[(&str, &str)] = &[\n",
    );
    for f in &files {
        println!("cargo:rerun-if-changed={}", f.display());
        let lang = f.file_stem().unwrap().to_string_lossy();
        let path = std::fs::canonicalize(f).unwrap();
        code.push_str(&format!(
            "    ({lang:?}, include_str!({:?})),\n",
            path.display().to_string()
        ));
    }
    code.push_str("];\n");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("builtin_synonyms.rs");
    std::fs::write(out, code).unwrap();
}
