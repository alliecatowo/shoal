//! Scope chain discovery (site/content/internals/reef-resolution.md).
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
    /// Advisory discovery diagnostics. A malformed/oversized manifest is
    /// skipped so farther scopes remain usable, but the reason is retained.
    pub warnings: Vec<String>,
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
        let mut warnings = Vec::new();
        let mut dir = Some(cwd);
        while let Some(d) = dir {
            collect_dir(d, &mut scopes, &mut warnings);
            dir = d.parent();
        }
        if let Some(user) = user_manifest {
            match crate::input::read_optional(user) {
                Ok(Some(text)) => match ReefManifest::parse_shoal_reef(&text) {
                    Ok(manifest) if !manifest.tools.is_empty() => push_scope(
                        &mut scopes,
                        &mut warnings,
                        ScopeEntry {
                            kind: ManifestKind::ShoalUser,
                            source: user.to_path_buf(),
                            mtime: mtime_of(user),
                            manifest,
                        },
                    ),
                    Ok(_) => {}
                    Err(error) => warnings.push(format!("{}: {error}", user.display())),
                },
                Ok(None) => {}
                Err(error) => warnings.push(error.to_string()),
            }
        }
        ScopeChain {
            cwd: cwd.to_path_buf(),
            scopes,
            warnings,
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

fn collect_dir(d: &Path, scopes: &mut Vec<ScopeEntry>, warnings: &mut Vec<String>) {
    let candidates: &[(&str, ManifestKind)] = &[
        (".reef.toml", ManifestKind::Reef),
        ("mise.toml", ManifestKind::Mise),
        (".mise.toml", ManifestKind::Mise),
        (".tool-versions", ManifestKind::ToolVersions),
    ];
    for (fname, kind) in candidates {
        let path = d.join(fname);
        let text = match crate::input::read_optional(&path) {
            Ok(Some(text)) => text,
            Ok(None) => continue,
            Err(error) => {
                warnings.push(error.to_string());
                continue;
            }
        };
        let parsed = match kind {
            ManifestKind::Reef => ReefManifest::parse_reef(&text),
            ManifestKind::Mise => ReefManifest::parse_mise(&text),
            ManifestKind::ToolVersions => ReefManifest::parse_tool_versions(&text),
            ManifestKind::ShoalUser => unreachable!("user scope not discovered by walk"),
        };
        match parsed {
            Ok(manifest) if !manifest.tools.is_empty() => push_scope(
                scopes,
                warnings,
                ScopeEntry {
                    kind: *kind,
                    source: path.clone(),
                    mtime: mtime_of(&path),
                    manifest,
                },
            ),
            Ok(_) => {}
            Err(error) => warnings.push(format!("{}: {error}", path.display())),
        }
    }
}

fn push_scope(scopes: &mut Vec<ScopeEntry>, warnings: &mut Vec<String>, entry: ScopeEntry) {
    if scopes.len() >= crate::input::REEF_MAX_SCOPES {
        if !warnings
            .iter()
            .any(|warning| warning.contains("scope identity limit"))
        {
            warnings.push(format!(
                "{}: scope identity limit reached ({})",
                entry.source.display(),
                crate::input::REEF_MAX_SCOPES
            ));
        }
        return;
    }
    scopes.push(entry);
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
    fn hostile_near_manifest_warns_and_far_valid_scope_survives() {
        let root = tempfile::tempdir().unwrap();
        write(&root.path().join(".reef.toml"), "[tools]\nnode='20'\n");
        let child = root.path().join("child");
        fs::create_dir_all(&child).unwrap();
        let hostile = child.join(".reef.toml");
        let file = fs::File::create(&hostile).unwrap();
        file.set_len((crate::input::REEF_MANIFEST_MAX_BYTES + 1) as u64)
            .unwrap();

        let chain = ScopeChain::discover(&child, None);
        assert_eq!(
            chain.nearest_for("node").unwrap().manifest.tools["node"].constraint,
            crate::version::Constraint::parse("20")
        );
        assert!(chain.warnings.iter().any(|warning| {
            warning.contains(&hostile.display().to_string()) && warning.contains("byte limit")
        }));
    }

    #[test]
    fn scope_identity_limit_is_explicit_and_bounded() {
        let mut scopes = Vec::new();
        let mut warnings = Vec::new();
        for index in 0..=crate::input::REEF_MAX_SCOPES {
            push_scope(
                &mut scopes,
                &mut warnings,
                ScopeEntry {
                    kind: ManifestKind::Reef,
                    source: PathBuf::from(format!("/scope-{index}/.reef.toml")),
                    manifest: ReefManifest::parse_reef("[tools]\nnode='1'\n").unwrap(),
                    mtime: None,
                },
            );
        }
        assert_eq!(scopes.len(), crate::input::REEF_MAX_SCOPES);
        assert_eq!(
            warnings
                .iter()
                .filter(|warning| warning.contains("scope identity limit"))
                .count(),
            1
        );
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
        filetime_set(&p, later).unwrap();
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
    fn filetime_set(p: &Path, t: SystemTime) -> std::io::Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let secs = match t.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(after) => after.as_secs() as libc::time_t,
            Err(before) => -(before.duration().as_secs() as libc::time_t),
        };
        let tv = libc::timeval {
            tv_sec: secs,
            tv_usec: 0,
        };
        let times = [tv, tv];
        let cpath = std::ffi::CString::new(p.as_os_str().as_bytes()).unwrap();
        // SAFETY: `cpath` is NUL-terminated and `times` contains two live
        // timeval values for the duration of the call.
        if unsafe { libc::utimes(cpath.as_ptr(), times.as_ptr()) } == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[test]
    fn pre_epoch_manifest_mtime_is_safe_cache_key_data() {
        let root = tempfile::tempdir().unwrap();
        let manifest = root.path().join(".reef.toml");
        write(&manifest, "[tools]\nnode = \"22\"\n");
        let before_epoch = SystemTime::UNIX_EPOCH - std::time::Duration::from_secs(1);
        if let Err(error) = filetime_set(&manifest, before_epoch) {
            eprintln!("filesystem does not support pre-epoch mtimes: {error}");
            return;
        }
        let chain = ScopeChain::discover(root.path(), None);
        assert_eq!(chain.scopes[0].mtime, Some(before_epoch));
        let _key = chain.key();
    }
}
