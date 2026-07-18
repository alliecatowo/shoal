//! cargo provider: `~/.cargo/bin` (or `$CARGO_HOME/bin`). Cargo binaries carry
//! no reliable version metadata, so their version is opaque-unknown; they only
//! satisfy `*`/`latest`. Never fetches.

use std::path::PathBuf;

use super::{Candidate, CandidateDiscovery, Provider, ProviderCtx, ProviderError, is_executable};
use crate::version::Version;

pub struct CargoProvider {
    bin_dir: PathBuf,
}

impl CargoProvider {
    pub fn new(bin_dir: PathBuf) -> CargoProvider {
        CargoProvider { bin_dir }
    }

    /// `$CARGO_HOME/bin` if set, else `~/.cargo/bin`.
    pub fn from_env() -> CargoProvider {
        let bin_dir = std::env::var_os("CARGO_HOME")
            .map(|h| PathBuf::from(h).join("bin"))
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo/bin")))
            .unwrap_or_else(|| PathBuf::from(".cargo/bin"));
        CargoProvider { bin_dir }
    }
}

impl Provider for CargoProvider {
    fn name(&self) -> &'static str {
        "cargo"
    }

    fn discover(
        &self,
        tool: &str,
        _ctx: &ProviderCtx,
    ) -> Result<CandidateDiscovery, ProviderError> {
        let path = self.bin_dir.join(tool);
        let mut discovery = CandidateDiscovery::new(self.name());
        if is_executable(&path) {
            discovery.push(Candidate::new(tool, Version::unknown(), path, "cargo"))?;
        }
        Ok(discovery)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn discovers_cargo_bin_as_unknown() {
        let home = tempfile::tempdir().unwrap();
        let bindir = home.path().join("bin");
        std::fs::create_dir_all(&bindir).unwrap();
        let bin = bindir.join("rg");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        let mut perm = std::fs::metadata(&bin).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&bin, perm).unwrap();

        let p = CargoProvider::new(bindir);
        let cands = p
            .discover("rg", &ProviderCtx::new("/"))
            .unwrap()
            .into_candidates();
        assert_eq!(cands.len(), 1);
        assert!(cands[0].version.is_unknown());
        assert_eq!(cands[0].provider, "cargo");
    }
}
