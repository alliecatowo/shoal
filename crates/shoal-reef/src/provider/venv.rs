//! venv provider: a project virtualenv's `.venv/bin/<tool>` when present. Walks
//! up from the cwd. Versions are opaque-unknown.

use super::{Candidate, CandidateDiscovery, Provider, ProviderCtx, ProviderError, is_executable};
use crate::version::Version;

pub struct VenvProvider;

impl VenvProvider {
    pub fn new() -> VenvProvider {
        VenvProvider
    }
}

impl Default for VenvProvider {
    fn default() -> Self {
        VenvProvider::new()
    }
}

impl Provider for VenvProvider {
    fn name(&self) -> &'static str {
        "venv"
    }

    fn discover(&self, tool: &str, ctx: &ProviderCtx) -> Result<CandidateDiscovery, ProviderError> {
        let mut discovery = CandidateDiscovery::new(self.name());
        let mut dir = Some(ctx.cwd.as_path());
        while let Some(d) = dir {
            let bin = d.join(".venv").join("bin").join(tool);
            if is_executable(&bin) {
                discovery.push(Candidate::new(tool, Version::unknown(), bin, "venv"))?;
                return Ok(discovery);
            }
            dir = d.parent();
        }
        Ok(discovery)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn finds_venv_bin() {
        let root = tempfile::tempdir().unwrap();
        let bindir = root.path().join(".venv/bin");
        std::fs::create_dir_all(&bindir).unwrap();
        let bin = bindir.join("black");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        let mut perm = std::fs::metadata(&bin).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&bin, perm).unwrap();

        let p = VenvProvider::new();
        let sub = root.path().join("pkg");
        std::fs::create_dir_all(&sub).unwrap();
        let cands = p
            .discover("black", &ProviderCtx::new(sub))
            .unwrap()
            .into_candidates();
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].provider, "venv");
    }
}
