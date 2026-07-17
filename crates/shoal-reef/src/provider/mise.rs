//! mise provider: enumerates `<data>/installs/<tool>/<version>/bin/<tool>`
//! directly — no shims, no `mise exec`, no forks. Honors `MISE_DATA_DIR`.
//! `fetch` shells out to `mise install tool@version` iff a `mise` binary exists.

use std::path::PathBuf;

use super::{Candidate, Provider, ProviderCtx, ProviderError, is_executable};
use crate::version::{Constraint, Version};

pub struct MiseProvider {
    data_dir: PathBuf,
}

impl MiseProvider {
    /// Explicit data dir (`<data>/installs/...`). Tests pass a fixture root.
    pub fn new(data_dir: PathBuf) -> MiseProvider {
        MiseProvider { data_dir }
    }

    /// `MISE_DATA_DIR` if set, else `~/.local/share/mise`.
    pub fn from_env() -> MiseProvider {
        let data_dir = std::env::var_os("MISE_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/mise"))
            })
            .unwrap_or_else(|| PathBuf::from(".local/share/mise"));
        MiseProvider { data_dir }
    }

    fn installs_root(&self) -> PathBuf {
        self.data_dir.join("installs")
    }

    /// mise plugins use both `<version>/<tool>` (downloaded binaries) and
    /// `<version>/bin/<tool>` (language/cargo installs). Backend-qualified
    /// plugin directories such as `cargo-cargo-audit` still expose the
    /// executable as `cargo-audit`.
    fn binary_for(&self, tool: &str, version_dir: &std::path::Path) -> Option<PathBuf> {
        [version_dir.join(tool), version_dir.join("bin").join(tool)]
            .into_iter()
            .find(|path| is_executable(path))
    }

    fn install_dirs(&self, tool: &str) -> Vec<PathBuf> {
        let root = self.installs_root();
        let mut dirs = vec![root.join(tool)];
        if matches!(tool, "cargo" | "rustc") {
            dirs.push(root.join("rust"));
        }
        let suffix = format!("-{tool}");
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten().take(1024) {
                let name = entry.file_name();
                if name.to_str().is_some_and(|name| name.ends_with(&suffix))
                    && !dirs.iter().any(|path| path == &entry.path())
                {
                    dirs.push(entry.path());
                }
            }
        }
        dirs
    }
}

impl Provider for MiseProvider {
    fn name(&self) -> &'static str {
        "mise"
    }

    fn discover(&self, tool: &str, _ctx: &ProviderCtx) -> Vec<Candidate> {
        let mut out = Vec::new();
        for dir in self.install_dirs(tool) {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten().take(1024) {
                let Ok(ft) = entry.file_type() else { continue };
                if !ft.is_dir() && !ft.is_symlink() {
                    continue;
                }
                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                if let Some(bin) = self.binary_for(tool, &entry.path()) {
                    // Version comes free from the directory name — no probe needed.
                    out.push(Candidate::new(tool, Version::parse(&name), bin, "mise"));
                }
            }
        }
        out
    }

    fn fetch(
        &self,
        tool: &str,
        req: &Constraint,
        ctx: &ProviderCtx,
    ) -> Option<Result<Candidate, ProviderError>> {
        // Only attempt if a `mise` binary is discoverable.
        which_mise()?;
        let spec = match req {
            Constraint::Any | Constraint::Latest => format!("{tool}@latest"),
            other => format!("{tool}@{other}"),
        };
        let status = std::process::Command::new("mise")
            .arg("install")
            .arg(&spec)
            .status();
        match status {
            Ok(s) if s.success() => {
                // Re-discover to pick up the freshly installed candidate.
                let best = self
                    .discover(tool, ctx)
                    .into_iter()
                    .filter(|c| req.satisfies(&c.version))
                    .max_by(|a, b| a.version.cmp(&b.version));
                match best {
                    Some(c) => Some(Ok(c)),
                    None => Some(Err(ProviderError::new(
                        "mise",
                        format!("installed {spec} but no satisfying candidate appeared"),
                    ))),
                }
            }
            Ok(s) => Some(Err(ProviderError::new(
                "mise",
                format!("mise install {spec} failed with status {s}"),
            ))),
            Err(e) => Some(Err(ProviderError::new(
                "mise",
                format!("mise install: {e}"),
            ))),
        }
    }
}

/// Locate a `mise` binary on the ambient PATH (used only to decide whether
/// `fetch` can run — never for resolution).
fn which_mise() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join("mise");
        if is_executable(&p) {
            return Some(p);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    /// Build a fake mise install layout: <data>/installs/<tool>/<ver>/bin/<tool>.
    fn install(data: &Path, tool: &str, ver: &str) -> PathBuf {
        let bindir = data.join("installs").join(tool).join(ver).join("bin");
        std::fs::create_dir_all(&bindir).unwrap();
        let bin = bindir.join(tool);
        let mut f = std::fs::File::create(&bin).unwrap();
        write!(f, "#!/bin/sh\necho {ver}\n").unwrap();
        let mut perm = std::fs::metadata(&bin).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&bin, perm).unwrap();
        bin
    }

    #[test]
    fn discovers_versions_from_dir_names() {
        let data = tempfile::tempdir().unwrap();
        install(data.path(), "node", "22.3.0");
        install(data.path(), "node", "20.11.1");
        let p = MiseProvider::new(data.path().into());
        let mut cands = p.discover("node", &ProviderCtx::new("/"));
        cands.sort_by(|a, b| b.version.cmp(&a.version));
        assert_eq!(cands.len(), 2);
        assert_eq!(cands[0].version.raw(), "22.3.0");
        assert_eq!(cands[1].version.raw(), "20.11.1");
        // No probe: versions came from directory names.
        assert!(cands.iter().all(|c| !c.version.is_unknown()));
    }

    #[test]
    fn missing_tool_yields_nothing() {
        let data = tempfile::tempdir().unwrap();
        let p = MiseProvider::new(data.path().into());
        assert!(p.discover("ghost", &ProviderCtx::new("/")).is_empty());
    }

    #[test]
    fn discovers_root_level_and_backend_qualified_binaries() {
        let data = tempfile::tempdir().unwrap();
        let root = data.path().join("installs/actionlint/1.7.12");
        std::fs::create_dir_all(&root).unwrap();
        let bin = root.join("actionlint");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        let mut permissions = std::fs::metadata(&bin).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&bin, permissions).unwrap();

        let cargo_root = data.path().join("installs/cargo-cargo-audit/0.22.2/bin");
        std::fs::create_dir_all(&cargo_root).unwrap();
        let cargo_bin = cargo_root.join("cargo-audit");
        std::fs::write(&cargo_bin, b"#!/bin/sh\n").unwrap();
        let mut permissions = std::fs::metadata(&cargo_bin).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&cargo_bin, permissions).unwrap();

        let provider = MiseProvider::new(data.path().into());
        assert_eq!(
            provider
                .discover("actionlint", &ProviderCtx::new("/"))
                .len(),
            1
        );
        assert_eq!(
            provider
                .discover("cargo-audit", &ProviderCtx::new("/"))
                .len(),
            1
        );
    }

    #[test]
    fn version_dir_without_binary_skipped() {
        let data = tempfile::tempdir().unwrap();
        // Create the version dir but no bin/<tool>.
        std::fs::create_dir_all(data.path().join("installs/node/9.9.9/bin")).unwrap();
        let p = MiseProvider::new(data.path().into());
        assert!(p.discover("node", &ProviderCtx::new("/")).is_empty());
    }
}
