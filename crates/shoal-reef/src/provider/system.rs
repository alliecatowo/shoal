//! System provider: scans canonical roots plus ambient `$PATH` dirs. Versions
//! are probed lazily via `<tool> --version` (300 ms timeout), parsed leniently,
//! and cached in-memory keyed by path. Enumeration never probes.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use super::{Candidate, Provider, ProviderCtx, is_executable, probe_version};
use crate::version::Version;

/// Canonical system roots, always scope `system` (not `ambient`).
pub const CANONICAL_ROOTS: &[&str] = &["/usr/bin", "/usr/local/bin", "/bin"];

pub struct SystemProvider {
    roots: Vec<PathBuf>,
    ambient: Vec<PathBuf>,
    cache: Mutex<HashMap<PathBuf, Version>>,
}

impl SystemProvider {
    /// Explicit roots + ambient dirs (tests pass fixture dirs here).
    pub fn new(roots: Vec<PathBuf>, ambient: Vec<PathBuf>) -> SystemProvider {
        SystemProvider { roots, ambient, cache: Mutex::new(HashMap::new()) }
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

    fn version_of(&self, cand: &Candidate) -> Version {
        if let Some(v) = self.cache.lock().unwrap().get(&cand.path) {
            return v.clone();
        }
        let v = probe_version(&cand.path);
        self.cache.lock().unwrap().insert(cand.path.clone(), v.clone());
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
        let v = p.version_of(&cands[0]);
        assert_eq!(v.raw(), "4.5.6");
        // Cached path present.
        assert!(p.cache.lock().unwrap().contains_key(&cands[0].path));
    }

    #[test]
    fn ignores_non_executable() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("plain"), b"not exec").unwrap();
        let p = SystemProvider::new(vec![root.path().into()], vec![]);
        assert!(p.discover("plain", &ProviderCtx::new("/")).is_empty());
    }
}
