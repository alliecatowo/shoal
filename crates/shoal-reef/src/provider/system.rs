//! System provider: scans canonical roots plus ambient `$PATH` dirs. Versions
//! are probed lazily via `<tool> --version` (300 ms timeout), parsed leniently,
//! and cached in-memory keyed by path. Enumeration never probes.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use super::{Candidate, Provider, ProviderCtx, is_executable, probe_version};
use crate::version::Version;

/// Canonical system roots, always scope `system` (not `ambient`).
pub const CANONICAL_ROOTS: &[&str] = &["/usr/bin", "/usr/local/bin", "/bin"];

/// Version probes are advisory and repeatable. Bound retained executable
/// identities so repeated PATH churn cannot grow a long-lived resolver.
const MAX_VERSION_CACHE_ENTRIES: usize = 1_024;

#[derive(Clone, PartialEq, Eq, Hash)]
struct VersionKey {
    path: PathBuf,
    dev: u64,
    ino: u64,
    mtime: i64,
    mtime_ns: i64,
    len: u64,
}

impl VersionKey {
    fn for_path(path: &std::path::Path) -> std::io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            dev: meta.dev(),
            ino: meta.ino(),
            mtime: meta.mtime(),
            mtime_ns: meta.mtime_nsec(),
            len: meta.len(),
        })
    }
}

pub struct SystemProvider {
    roots: Vec<PathBuf>,
    ambient: Vec<PathBuf>,
    cache: Mutex<HashMap<VersionKey, Version>>,
}

impl SystemProvider {
    /// Explicit roots + ambient dirs (tests pass fixture dirs here).
    pub fn new(roots: Vec<PathBuf>, ambient: Vec<PathBuf>) -> SystemProvider {
        SystemProvider {
            roots,
            ambient,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Canonical roots plus the ambient dirs parsed from `$PATH` (dirs that are
    /// not canonical roots are marked ambient).
    pub fn from_env() -> SystemProvider {
        let roots: Vec<PathBuf> = CANONICAL_ROOTS.iter().map(PathBuf::from).collect();
        let mut ambient = Vec::new();
        if let Some(path) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&path) {
                if !roots.iter().any(|r| r == &dir) && !ambient.contains(&dir) {
                    ambient.push(dir);
                }
            }
        }
        SystemProvider::new(roots, ambient)
    }

    fn lock_cache(&self) -> MutexGuard<'_, HashMap<VersionKey, Version>> {
        match self.cache.lock() {
            Ok(cache) => cache,
            Err(poisoned) => {
                // Version probes are advisory and repeatable. Never trust a
                // partially-mutated cache left by a panic; clear and rebuild.
                let mut cache = poisoned.into_inner();
                cache.clear();
                self.cache.clear_poison();
                cache
            }
        }
    }
}

impl Provider for SystemProvider {
    fn name(&self) -> &'static str {
        "system"
    }

    fn discover(&self, tool: &str, _ctx: &ProviderCtx) -> Vec<Candidate> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for (dirs, ambient) in [(&self.roots, false), (&self.ambient, true)] {
            for dir in dirs {
                let path = dir.join(tool);
                if is_executable(&path) && seen.insert(path.clone()) {
                    let mut c = Candidate::new(tool, Version::unknown(), path, "system");
                    c.ambient = ambient;
                    out.push(c);
                }
            }
        }
        out
    }

    fn version_of(&self, cand: &Candidate, ctx: &ProviderCtx) -> Version {
        let Ok(key) = VersionKey::for_path(&cand.path) else {
            return probe_version(&cand.path, ctx);
        };
        if let Some(v) = self.lock_cache().get(&key) {
            return v.clone();
        }
        let v = probe_version(&cand.path, ctx);
        let mut cache = self.lock_cache();
        if cache.len() >= MAX_VERSION_CACHE_ENTRIES && !cache.contains_key(&key) {
            cache.clear();
        }
        cache.insert(key, v.clone());
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn make_exe(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        write!(f, "{body}").unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&p, perm).unwrap();
        p
    }

    #[test]
    fn discover_finds_executables_no_probe() {
        let root = tempfile::tempdir().unwrap();
        let amb = tempfile::tempdir().unwrap();
        make_exe(root.path(), "mytool", "#!/bin/sh\necho 1.0.0\n");
        make_exe(amb.path(), "mytool", "#!/bin/sh\necho 2.0.0\n");
        let p = SystemProvider::new(vec![root.path().into()], vec![amb.path().into()]);
        let cands = p.discover("mytool", &ProviderCtx::new("/"));
        assert_eq!(cands.len(), 2);
        // discover must not probe: versions stay unknown.
        assert!(cands.iter().all(|c| c.version.is_unknown()));
        let ambient_marked = cands.iter().find(|c| c.ambient).unwrap();
        assert!(ambient_marked.path.starts_with(amb.path()));
    }

    #[test]
    fn version_probe_and_cache() {
        let root = tempfile::tempdir().unwrap();
        make_exe(root.path(), "probed", "#!/bin/sh\necho 'probed 4.5.6'\n");
        let p = SystemProvider::new(vec![root.path().into()], vec![]);
        let cands = p.discover("probed", &ProviderCtx::new("/"));
        let v = p.version_of(&cands[0], &ProviderCtx::new("/"));
        assert_eq!(v.raw(), "4.5.6");
        // Cached path present.
        assert!(
            p.lock_cache()
                .contains_key(&VersionKey::for_path(&cands[0].path).unwrap())
        );
    }

    #[test]
    fn ignores_non_executable() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("plain"), b"not exec").unwrap();
        let p = SystemProvider::new(vec![root.path().into()], vec![]);
        assert!(p.discover("plain", &ProviderCtx::new("/")).is_empty());
    }

    #[test]
    fn poisoned_version_cache_discards_untrusted_entries_and_reprobes() {
        let root = tempfile::tempdir().unwrap();
        make_exe(root.path(), "probed", "#!/bin/sh\necho 'probed 4.5.6'\n");
        let provider = std::sync::Arc::new(SystemProvider::new(vec![root.path().into()], vec![]));
        let candidate = provider.discover("probed", &ProviderCtx::new("/"))[0].clone();
        let poison_target = provider.clone();
        let poisoned_path = candidate.path.clone();
        let poisoner = std::thread::Builder::new()
            .name("poison-system-version-cache".into())
            .spawn(move || {
                let mut cache = poison_target
                    .cache
                    .lock()
                    .expect("version cache starts healthy");
                cache.insert(
                    VersionKey::for_path(&poisoned_path).unwrap(),
                    Version::parse("99.99.99"),
                );
                panic!("inject system version cache poison");
            })
            .expect("spawn version cache poisoner");
        assert!(poisoner.join().is_err());

        assert_eq!(
            provider
                .version_of(&candidate, &ProviderCtx::new("/"))
                .raw(),
            "4.5.6"
        );
        assert!(!provider.cache.is_poisoned());
        assert_eq!(provider.lock_cache().len(), 1);
    }

    #[test]
    fn executable_replacement_invalidates_the_cached_version() {
        let root = tempfile::tempdir().unwrap();
        let path = make_exe(
            root.path(),
            "changing",
            "#!/bin/sh\necho 'changing 1.0.0'\n",
        );
        let provider = SystemProvider::new(vec![root.path().into()], vec![]);
        let candidate = provider.discover("changing", &ProviderCtx::new("/"))[0].clone();
        assert_eq!(
            provider
                .version_of(&candidate, &ProviderCtx::new("/"))
                .raw(),
            "1.0.0"
        );

        // A different length is part of the identity even on filesystems with
        // coarse mtimes, so an in-place replacement must be reprobed.
        make_exe(
            root.path(),
            "changing",
            "#!/bin/sh\necho 'changing 22.33.44'\n",
        );
        assert_eq!(
            provider
                .version_of(&candidate, &ProviderCtx::new("/"))
                .raw(),
            "22.33.44"
        );
        assert_eq!(path, candidate.path);
    }

    #[test]
    fn version_cache_churn_clears_at_its_ceiling() {
        let provider = SystemProvider::new(Vec::new(), Vec::new());
        let mut cache = provider.lock_cache();
        for ino in 0..MAX_VERSION_CACHE_ENTRIES as u64 {
            cache.insert(
                VersionKey {
                    path: PathBuf::from(format!("/old/{ino}")),
                    dev: 1,
                    ino,
                    mtime: 1,
                    mtime_ns: 0,
                    len: 1,
                },
                Version::parse("1.0.0"),
            );
        }
        drop(cache);

        let root = tempfile::tempdir().unwrap();
        make_exe(root.path(), "current", "#!/bin/sh\necho 'current 3.2.1'\n");
        // Use the same provider so admission exercises its full cache.
        let candidate = Candidate::new(
            "current",
            Version::unknown(),
            root.path().join("current"),
            "system",
        );
        assert_eq!(
            provider
                .version_of(&candidate, &ProviderCtx::new("/"))
                .raw(),
            "3.2.1"
        );
        assert_eq!(provider.lock_cache().len(), 1);
    }

    #[test]
    fn production_system_cache_has_no_panicking_lock_access() {
        let production = include_str!("system.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("production source prefix");
        let compact = production.split_whitespace().collect::<String>();
        for forbidden in [".lock().unwrap(", ".lock().expect("] {
            assert!(
                !compact.contains(forbidden),
                "production system cache synchronization contains `{forbidden}`"
            );
        }
    }
}
