//! Embeds the repository's `adapters/*.toml` manifests so installed binaries are self-contained.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::{env, fs};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let adapters = manifest_dir.join("../../adapters");
    println!("cargo:rerun-if-changed={}", adapters.display());

    let mut files: Vec<PathBuf> = fs::read_dir(&adapters)
        .unwrap_or_else(|e| panic!("read {}: {e}", adapters.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    files.sort();

    let mut out = String::from("pub(crate) const MANIFESTS: &[(&str, &str)] = &[\n");
    for file in &files {
        let canonical = file.canonicalize().expect("canonicalize adapter manifest");
        println!("cargo:rerun-if-changed={}", canonical.display());
        let name = file.file_name().expect("file name").to_string_lossy();
        writeln!(
            out,
            "    ({name:?}, include_str!({:?})),",
            canonical.display().to_string()
        )
        .expect("write to string");
    }
    out.push_str("];\n");

    let dest = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR")).join("bundled_adapters.rs");
    fs::write(dest, out).expect("write bundled_adapters.rs");
}
