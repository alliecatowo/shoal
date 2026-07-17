//! The lock — `reef.lock`, a committed TOML file recording every resolved tool
//! (site/content/internals/reef-resolution.md). Lives next to the project manifest, or in the user state dir
//! for the user scope.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One locked binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[serde(deny_unknown_fields)]
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
        let Some(text) = crate::input::read_optional(path).map_err(lock_error)? else {
            return Ok(Lockfile::new());
        };
        crate::input::validate_toml_text(&text).map_err(lock_error)?;
        let lock: Lockfile = toml::from_str(&text).map_err(lock_error)?;
        lock.validate()?;
        Ok(lock)
    }

    /// Serialize to a TOML string.
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// Write the lockfile to disk (creating parent dirs).
    pub fn save(&self, path: &Path) -> Result<(), LockError> {
        self.validate()?;
        let text = self.to_toml();
        if text.len() > crate::input::REEF_MANIFEST_MAX_BYTES {
            return Err(lock_error(format!(
                "serialized lock exceeds the {}-byte limit",
                crate::input::REEF_MANIFEST_MAX_BYTES
            )));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| LockError { msg: e.to_string() })?;
        }
        std::fs::write(path, text).map_err(lock_error)
    }

    pub fn get(&self, name: &str) -> Option<&LockEntry> {
        self.tools.get(name)
    }

    pub fn insert(&mut self, entry: LockEntry) {
        let _ = self.try_insert(entry);
    }

    pub fn try_insert(&mut self, entry: LockEntry) -> Result<(), LockError> {
        if !self.tools.contains_key(&entry.name)
            && self.tools.len() >= crate::input::REEF_LOCK_MAX_TOOLS
        {
            return Err(lock_error(format!(
                "lock tool identity limit reached ({})",
                crate::input::REEF_LOCK_MAX_TOOLS
            )));
        }
        validate_entry(&entry)?;
        self.tools.insert(entry.name.clone(), entry);
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Option<LockEntry> {
        self.tools.remove(name)
    }

    fn validate(&self) -> Result<(), LockError> {
        if self.tools.len() > crate::input::REEF_LOCK_MAX_TOOLS {
            return Err(lock_error(format!(
                "lock has {} tools; maximum is {}",
                self.tools.len(),
                crate::input::REEF_LOCK_MAX_TOOLS
            )));
        }
        for entry in self.tools.values() {
            validate_entry(entry)?;
        }
        Ok(())
    }
}

fn validate_entry(entry: &LockEntry) -> Result<(), LockError> {
    let path = entry.path.to_string_lossy();
    for (kind, value) in [
        ("lock name", entry.name.as_str()),
        ("lock version", entry.version.as_str()),
        ("lock provider", entry.provider.as_str()),
        ("lock path", path.as_ref()),
        ("lock hash", entry.blake3.as_str()),
        ("lock timestamp", entry.resolved_at.as_str()),
    ] {
        crate::input::validate_string(kind, value).map_err(lock_error)?;
    }
    Ok(())
}

fn lock_error(error: impl std::fmt::Display) -> LockError {
    LockError {
        msg: error.to_string(),
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

    #[test]
    fn hostile_lockfiles_fail_without_replacing_missing_semantics() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("reef.lock");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len((crate::input::REEF_MANIFEST_MAX_BYTES + 1) as u64)
            .unwrap();
        assert!(
            Lockfile::load(&path)
                .unwrap_err()
                .msg
                .contains("byte limit")
        );

        std::fs::write(&path, [0xff]).unwrap();
        assert!(Lockfile::load(&path).unwrap_err().msg.contains("UTF-8"));
        std::fs::write(
            &path,
            "[tool.node]\nname='node'\nname='other'\nversion='1'\nprovider='p'\npath='/x'\nblake3='x'\nresolved_at='now'\n",
        )
        .unwrap();
        assert!(Lockfile::load(&path).is_err());
        std::fs::write(
            &path,
            "[tool.node]\nname='node'\nversion='1'\nprovider='p'\npath='/x'\nblake3='x'\nresolved_at='now'\nunknown=true\n",
        )
        .unwrap();
        assert!(Lockfile::load(&path).is_err());
    }

    #[test]
    fn lock_identity_and_string_limits_are_enforced_transactionally() {
        let mut lock = Lockfile::new();
        for index in 0..crate::input::REEF_LOCK_MAX_TOOLS {
            lock.try_insert(entry(&format!("tool-{index}"))).unwrap();
        }
        let error = lock.try_insert(entry("overflow")).unwrap_err();
        assert!(error.msg.contains("identity limit"));
        assert!(lock.get("overflow").is_none());

        let mut huge = entry("huge");
        huge.provider = "x".repeat(crate::input::REEF_MAX_STRING_BYTES + 1);
        assert!(Lockfile::new().try_insert(huge).is_err());
    }
}
