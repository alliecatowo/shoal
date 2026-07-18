//! mise provider: enumerates `<data>/installs/<tool>/<version>/bin/<tool>`
//! directly — no shims, no `mise exec`, no forks. Honors `MISE_DATA_DIR`.
//! `fetch` shells out to `mise install tool@version` iff a `mise` binary exists.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::time::Duration;

use super::{Candidate, Provider, ProviderCtx, ProviderError, is_executable};
use crate::version::{Constraint, Version};

const FETCH_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const FETCH_OUTPUT_CAP: usize = 256 * 1024;
const FETCH_ERROR_PREVIEW: usize = 4 * 1024;

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
        // Only attempt if a `mise` binary is discoverable through the exact
        // provider context that will execute it.
        let mise = ctx.which(OsStr::new("mise"))?;
        let spec = match req {
            Constraint::Any | Constraint::Latest => format!("{tool}@latest"),
            other => format!("{tool}@{other}"),
        };
        let args = [OsString::from("install"), OsString::from(&spec)];
        let output = ctx.run(&mise, &args, FETCH_TIMEOUT, FETCH_OUTPUT_CAP);
        match output {
            Ok(output) if output.status.success() && !output.timed_out => {
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
            Ok(output) if output.timed_out => Some(Err(ProviderError::new(
                "mise",
                format!("mise install {spec} exceeded {FETCH_TIMEOUT:?}"),
            ))),
            Ok(output) => {
                let mut detail = output.stderr;
                if detail.is_empty() {
                    detail = output.stdout;
                }
                detail.truncate(FETCH_ERROR_PREVIEW);
                let detail = String::from_utf8_lossy(&detail);
                Some(Err(ProviderError::new(
                    "mise",
                    format!(
                        "mise install {spec} failed with status {}: {detail}",
                        output.status
                    ),
                )))
            }
            Err(error) => Some(Err(ProviderError::new(
                "mise",
                format!("mise install {spec}: {error}"),
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ProviderCommand, ProviderRunner};
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use std::path::Path;
    use std::sync::{Arc, Mutex};

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

    struct InstallRunner {
        data: PathBuf,
        calls: Mutex<Vec<InstallCall>>,
    }

    #[derive(Debug, PartialEq, Eq)]
    struct InstallCall {
        program: PathBuf,
        args: Vec<OsString>,
        timeout: Duration,
        output_cap: usize,
    }

    impl ProviderRunner for InstallRunner {
        fn run(
            &self,
            command: ProviderCommand<'_>,
        ) -> std::io::Result<shoal_exec::BoundedCommandOutput> {
            self.calls.lock().unwrap().push(InstallCall {
                program: command.program.to_path_buf(),
                args: command.args.to_vec(),
                timeout: command.timeout,
                output_cap: command.output_cap,
            });
            install(&self.data, "guarded", "1.2.3");
            Ok(shoal_exec::BoundedCommandOutput {
                status: std::process::ExitStatus::from_raw(0),
                stdout: Vec::new(),
                stderr: Vec::new(),
                truncated: false,
                timed_out: false,
                pgid: 1,
                duration: Duration::ZERO,
            })
        }
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
    fn fetch_uses_context_runner_and_bounded_installer_budget() {
        let data = tempfile::tempdir().unwrap();
        let helpers = tempfile::tempdir().unwrap();
        let mise = helpers.path().join("mise");
        std::fs::write(&mise, b"fixture").unwrap();
        let mut permissions = std::fs::metadata(&mise).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&mise, permissions).unwrap();

        let runner = Arc::new(InstallRunner {
            data: data.path().to_path_buf(),
            calls: Mutex::new(Vec::new()),
        });
        let context = ProviderCtx::with_runner(
            data.path(),
            Some(std::env::join_paths([helpers.path()]).unwrap()),
            runner.clone(),
        );
        let provider = MiseProvider::new(data.path().to_path_buf());
        let candidate = provider
            .fetch("guarded", &Constraint::parse("1.2.3"), &context)
            .expect("mise can fetch")
            .expect("bounded installer succeeds");
        assert_eq!(candidate.version.raw(), "1.2.3");
        assert_eq!(
            *runner.calls.lock().unwrap(),
            vec![InstallCall {
                program: mise,
                args: vec![OsString::from("install"), OsString::from("guarded@1.2.3")],
                timeout: FETCH_TIMEOUT,
                output_cap: FETCH_OUTPUT_CAP,
            }]
        );
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
