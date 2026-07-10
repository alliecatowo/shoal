//! Scope chain discovery (REEF.md §1).
//!
//! [`ScopeChain::discover`] is a **pure function of `(cwd, filesystem)`**: it
//! walks up from a directory collecting reef manifests (native and foreign),
//! then appends the user scope. No activation, no hooks, no env mutation. `cd`
//! re-scopes the next resolution and nothing else.
//!
//! The chain is ordered nearest-first. Within a single directory, a native
//! `.reef.toml` is ordered before foreign manifests so it wins. The chain
//! records each manifest's path and mtime so callers can cache a chain and
//! detect staleness by comparing the [`ChainKey`] (paths + mtimes) — there is no
//! internal cache to invalidate.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::manifest::{ManifestKind, ReefManifest};

/// One scope in the chain.
#[derive(Debug, Clone)]
pub struct ScopeEntry {
    pub kind: ManifestKind,
    /// Absolute path to the manifest file.
    pub source: PathBuf,
    pub manifest: ReefManifest,
    /// mtime of the manifest file at discovery time (for cache keying).
    pub mtime: Option<SystemTime>,
}

impl ScopeEntry {
    /// A short human label for diagnostics (`"reef"`, `"mise"`, `"user"`…).
    pub fn label(&self) -> &'static str {
        self.kind.as_str()
    }
}

/// An ordered, nearest-first chain of scopes.
#[derive(Debug, Clone, Default)]
pub struct ScopeChain {
    pub cwd: PathBuf,
    pub scopes: Vec<ScopeEntry>,
}

/// A cache key: the set of (path, mtime) pairs that produced a chain. Two chains
/// with equal keys are guaranteed identical, so a caller can key its own cache
/// on this and never suffer a stale-manifest bug.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainKey {
    entries: Vec<(PathBuf, Option<SystemTime>)>,
}

impl ScopeChain {
    /// Discover the scope chain for `cwd`.
    ///
    /// - `user_manifest`: optional path to the user's `shoal.toml` (its `[reef]`
    ///   table becomes the user scope). Pass `None` to skip the user scope
    ///   (tests, hermetic contexts).
    ///
    /// Walks from `cwd` to the filesystem root. At each directory it reads, in
    /// order: `.reef.toml`, `mise.toml`, `.mise.toml`, `.tool-versions`.
    /// Unreadable or malformed manifests are skipped (best-effort discovery;
    /// use [`ReefManifest::parse_reef`] directly to surface parse errors).
    pub fn discover(cwd: &Path, user_manifest: Option<&Path>) -> ScopeChain {
        let mut scopes = Vec::new();
        let mut dir = Some(cwd);
        while let Some(d) = dir {
            collect_dir(d, &mut scopes);
            dir = d.parent();
        }
        if let Some(user) = user_manifest
            && let Ok(text) = std::fs::read_to_string(user)
            && let Ok(manifest) = ReefManifest::parse_shoal_reef(&text)
            && !manifest.tools.is_empty()
        {
            scopes.push(ScopeEntry {
                kind: ManifestKind::ShoalUser,
                source: user.to_path_buf(),
                mtime: mtime_of(user),
                manifest,
            });
        }
        ScopeChain {
            cwd: cwd.to_path_buf(),
            scopes,
        }
    }

    /// The cache key for this chain (paths + mtimes).
    pub fn key(&self) -> ChainKey {
        ChainKey {
            entries: self
                .scopes
                .iter()
                .map(|s| (s.source.clone(), s.mtime))
                .collect(),
        }
    }

    /// The nearest scope that constrains `tool`, if any.
    pub fn nearest_for(&self, tool: &str) -> Option<&ScopeEntry> {
        self.scopes
            .iter()
            .find(|s| s.manifest.tools.contains_key(tool))
    }

    /// Merge every scope's runner table (farthest first so nearest wins), atop
    /// the shipped defaults.
    pub fn runner_table(&self) -> crate::runner::RunnerTable {
        let mut table = crate::runner::RunnerTable::defaults();
        for scope in self.scopes.iter().rev() {
            if !scope.manifest.runners.is_empty() {
                table.overlay(&scope.manifest.runners);
            }
        }
        table
    }

    /// Whether any scope requests a hermetic child PATH.
    pub fn hermetic(&self) -> bool {
        self.scopes.iter().any(|s| s.manifest.hermetic)
    }
}

fn collect_dir(d: &Path, scopes: &mut Vec<ScopeEntry>) {
    let candidates: &[(&str, ManifestKind)] = &[
        (".reef.toml", ManifestKind::Reef),
        ("mise.toml", ManifestKind::Mise),
        (".mise.toml", ManifestKind::Mise),
        (".tool-versions", ManifestKind::ToolVersions),
    ];
    for (fname, kind) in candidates {
        let path = d.join(fname);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let parsed = match kind {
            ManifestKind::Reef => ReefManifest::parse_reef(&text),
            ManifestKind::Mise => ReefManifest::parse_mise(&text),
            ManifestKind::ToolVersions => ReefManifest::parse_tool_versions(&text),
            ManifestKind::ShoalUser => unreachable!("user scope not discovered by walk"),
        };
        if let Ok(manifest) = parsed
            && !manifest.tools.is_empty()
        {
            scopes.push(ScopeEntry {
                kind: *kind,
                source: path.clone(),
                mtime: mtime_of(&path),
                manifest,
            });
        }
    }
}

fn mtime_of(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).ok().and_then(|m| m.modified().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(p: &Path, text: &str) {
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(p, text).unwrap();
    }

    #[test]
    fn nearest_project_wins() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        write(&base.join(".reef.toml"), "[tools]\nnode = \"18\"\n");
        write(&base.join("a/b/.reef.toml"), "[tools]\nnode = \"22\"\n");
        let chain = ScopeChain::discover(&base.join("a/b/c"), None);
        let near = chain.nearest_for("node").unwrap();
        assert_eq!(
            near.manifest.tools["node"].constraint,
            crate::version::Constraint::parse("22")
        );
    }

    #[test]
    fn reef_beats_mise_in_same_dir() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        write(&base.join(".reef.toml"), "[tools]\nnode = \"22\"\n");
        write(&base.join("mise.toml"), "[tools]\nnode = \"18\"\n");
        let chain = ScopeChain::discover(base, None);
        // The reef entry appears before the mise entry, so nearest_for returns it.
        let near = chain.nearest_for("node").unwrap();
        assert_eq!(near.kind, ManifestKind::Reef);
        assert_eq!(
            near.manifest.tools["node"].constraint,
            crate::version::Constraint::parse("22")
        );
    }

    #[test]
    fn foreign_only_discovered() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        write(&base.join(".tool-versions"), "nodejs 20.1.0\n");
        let chain = ScopeChain::discover(base, None);
        let near = chain.nearest_for("nodejs").unwrap();
        assert_eq!(near.kind, ManifestKind::ToolVersions);
    }

    #[test]
    fn user_scope_appended_last() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        write(&base.join(".reef.toml"), "[tools]\nnode = \"22\"\n");
        let user = base.join("home/shoal.toml");
        write(&user, "[reef.tools]\npython = \"3.12\"\n");
        let chain = ScopeChain::discover(base, Some(&user));
        assert!(chain.nearest_for("python").unwrap().kind == ManifestKind::ShoalUser);
        // Project scope is nearer than user scope.
        let idx_proj = chain
            .scopes
            .iter()
            .position(|s| s.kind == ManifestKind::Reef)
            .unwrap();
        let idx_user = chain
            .scopes
            .iter()
            .position(|s| s.kind == ManifestKind::ShoalUser)
            .unwrap();
        assert!(idx_proj < idx_user);
    }

    #[test]
    fn runner_table_merges_nearest_over_defaults() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        // Farther scope overrides `py`; nearer scope overrides it again.
        write(
            &base.join(".reef.toml"),
            "[tools]\nnode = \"22\"\n[runners]\npy = \"python2\"\n",
        );
        let sub = base.join("proj");
        write(
            &sub.join(".reef.toml"),
            "[tools]\nnode = \"22\"\n[runners]\npy = \"python3\"\nrb = \"jruby\"\n",
        );
        let chain = ScopeChain::discover(&sub, None);
        let table = chain.runner_table();
        // Nearest (`proj`) wins over the farther override.
        assert_eq!(table.get("py").unwrap().tool, "python3");
        // A scope-added runner not present in the shipped defaults appears.
        assert_eq!(table.get("rb").unwrap().tool, "jruby");
        // Shipped defaults still present for extensions no scope touched.
        assert_eq!(table.get("js").unwrap().tool, "node");
    }

    #[test]
    fn hermetic_true_if_any_scope_requests_it() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        write(&base.join(".reef.toml"), "[tools]\nnode = \"22\"\n");
        let sub = base.join("proj");
        write(
            &sub.join(".reef.toml"),
            "[tools]\npython = \"3\"\n[options]\nhermetic = true\n",
        );
        let chain = ScopeChain::discover(&sub, None);
        assert!(chain.hermetic(), "any scope requesting hermetic wins");
    }

    #[test]
    fn hermetic_false_when_no_scope_requests_it() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        write(&base.join(".reef.toml"), "[tools]\nnode = \"22\"\n");
        let chain = ScopeChain::discover(base, None);
        assert!(!chain.hermetic());
    }

    #[test]
    fn chain_key_reflects_mtime_change() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        let p = base.join(".reef.toml");
        write(&p, "[tools]\nnode = \"22\"\n");
        let k1 = ScopeChain::discover(base, None).key();
        // Bump mtime forward deterministically.
        let later = SystemTime::now() + std::time::Duration::from_secs(120);
        filetime_set(&p, later);
        let k2 = ScopeChain::discover(base, None).key();
        assert_ne!(k1, k2, "mtime change must change the chain key");
    }

    #[test]
    fn chain_key_stable_when_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path();
        write(&base.join(".reef.toml"), "[tools]\nnode = \"22\"\n");
        let k1 = ScopeChain::discover(base, None).key();
        let k2 = ScopeChain::discover(base, None).key();
        assert_eq!(k1, k2);
    }

    // Minimal mtime setter using libc utimes (avoids a filetime dependency).
    fn filetime_set(p: &Path, t: SystemTime) {
        use std::os::unix::ffi::OsStrExt;
        let secs = t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as libc::time_t;
        let tv = libc::timeval {
            tv_sec: secs,
            tv_usec: 0,
        };
        let times = [tv, tv];
        let cpath = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        unsafe {
            libc::utimes(cpath.as_ptr(), times.as_ptr());
        }
    }
}
