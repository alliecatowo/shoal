//! The lock — `reef.lock`, a committed TOML file recording every resolved tool
//! (site/content/internals/reef-resolution.md). Lives next to the project manifest, or in the user state dir
//! for the user scope.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One locked binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockEntry {
    pub name: String,
    pub version: String,
    pub provider: String,
    pub path: PathBuf,
    /// blake3 hex of the binary at lock time.
    pub blake3: String,
    /// RFC3339 UTC timestamp.
    pub resolved_at: String,
}

/// A parsed `reef.lock`: tool name → entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lockfile {
    #[serde(default, rename = "tool")]
    pub tools: BTreeMap<String, LockEntry>,
}

impl Lockfile {
    pub fn new() -> Lockfile {
        Lockfile::default()
    }

    /// The conventional lockfile path next to a manifest file: `<dir>/reef.lock`.
    pub fn path_next_to(manifest: &Path) -> PathBuf {
        manifest
            .parent()
            .unwrap_or(Path::new("."))
            .join("reef.lock")
    }

    /// Load a lockfile from disk. A missing file yields an empty lockfile;
    /// malformed TOML is an error.
    pub fn load(path: &Path) -> Result<Lockfile, LockError> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|e| LockError { msg: e.to_string() }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Lockfile::new()),
            Err(e) => Err(LockError { msg: e.to_string() }),
        }
    }

    /// Serialize to a TOML string.
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// Write the lockfile to disk (creating parent dirs).
    pub fn save(&self, path: &Path) -> Result<(), LockError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| LockError { msg: e.to_string() })?;
        }
        std::fs::write(path, self.to_toml()).map_err(|e| LockError { msg: e.to_string() })
    }

    pub fn get(&self, name: &str) -> Option<&LockEntry> {
        self.tools.get(name)
    }

    pub fn insert(&mut self, entry: LockEntry) {
        self.tools.insert(entry.name.clone(), entry);
    }

    pub fn remove(&mut self, name: &str) -> Option<LockEntry> {
        self.tools.remove(name)
    }
}

/// Error loading/saving a lockfile.
#[derive(Debug, Clone)]
pub struct LockError {
    pub msg: String,
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "lock error: {}", self.msg)
    }
}
impl std::error::Error for LockError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> LockEntry {
        LockEntry {
            name: name.into(),
            version: "22.3.0".into(),
            provider: "mise".into(),
            path: PathBuf::from("/x/node"),
            blake3: "abc123".into(),
            resolved_at: "2026-07-09T00:00:00Z".into(),
        }
    }

    #[test]
    fn roundtrip_through_toml() {
        let mut lf = Lockfile::new();
        lf.insert(entry("node"));
        lf.insert(entry("python"));
        let text = lf.to_toml();
        let back: Lockfile = toml::from_str(&text).unwrap();
        assert_eq!(lf, back);
        assert_eq!(back.get("node").unwrap().version, "22.3.0");
    }

    #[test]
    fn save_and_load_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("reef.lock");
        let mut lf = Lockfile::new();
        lf.insert(entry("node"));
        lf.save(&p).unwrap();
        let back = Lockfile::load(&p).unwrap();
        assert_eq!(lf, back);
    }

    #[test]
    fn missing_file_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let lf = Lockfile::load(&dir.path().join("nope.lock")).unwrap();
        assert!(lf.tools.is_empty());
    }

    #[test]
    fn path_next_to_manifest() {
        let p = Lockfile::path_next_to(Path::new("/proj/.reef.toml"));
        assert_eq!(p, PathBuf::from("/proj/reef.lock"));
    }
}
