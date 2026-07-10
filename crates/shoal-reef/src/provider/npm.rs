//! npm-local provider: the thing everyone hacks PATH for, made a declared,
//! scoped provider. Walks up from the cwd for `node_modules/.bin/<tool>`.
//! Versions are opaque-unknown (npm bin scripts have no stable `--version`
//! contract worth probing here).

use super::{Candidate, Provider, ProviderCtx, is_executable};
use crate::version::Version;

pub struct NpmLocalProvider;

impl NpmLocalProvider {
    pub fn new() -> NpmLocalProvider {
        NpmLocalProvider
    }
}

impl Default for NpmLocalProvider {
    fn default() -> Self {
        NpmLocalProvider::new()
    }
}

impl Provider for NpmLocalProvider {
    fn name(&self) -> &'static str {
        "npm-local"
    }

    fn discover(&self, tool: &str, ctx: &ProviderCtx) -> Vec<Candidate> {
        let mut dir = Some(ctx.cwd.as_path());
        while let Some(d) = dir {
            let bin = d.join("node_modules").join(".bin").join(tool);
            if is_executable(&bin) {
                return vec![Candidate::new(tool, Version::unknown(), bin, "npm-local")];
            }
            dir = d.parent();
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    fn make_bin(dir: &std::path::Path, tool: &str) -> PathBuf {
        let bindir = dir.join("node_modules/.bin");
        std::fs::create_dir_all(&bindir).unwrap();
        let bin = bindir.join(tool);
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        let mut perm = std::fs::metadata(&bin).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&bin, perm).unwrap();
        bin
    }

    #[test]
    fn finds_node_modules_bin_walking_up() {
        let root = tempfile::tempdir().unwrap();
        make_bin(root.path(), "eslint");
        let deep = root.path().join("src/components");
        std::fs::create_dir_all(&deep).unwrap();
        let p = NpmLocalProvider::new();
        let cands = p.discover("eslint", &ProviderCtx::new(deep));
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].provider, "npm-local");
    }

    #[test]
    fn absent_when_no_node_modules() {
        let root = tempfile::tempdir().unwrap();
        let p = NpmLocalProvider::new();
        assert!(p.discover("eslint", &ProviderCtx::new(root.path())).is_empty());
    }
}
