//! Evaluator-side port adapters. See `site/content/internals/effects-plans-security.md`
//! and `site/content/internals/intercrate-protocol-contracts.md`.
//!
//! The port *traits* live in `shoal-value` so the domain core depends only on
//! them. The two adapters here need workspace crates `shoal-value` cannot see
//! (`shoal-exec`, `shoal-secret`), so they are implemented in `shoal-eval` — the
//! composition point where those concrete deps already exist. Both perform the
//! *exact* calls the evaluator made inline before the ports existed, so they are
//! byte-identical defaults.

use shoal_exec::{CancelToken, ExecResult, ExecSpec};
use shoal_value::SecretPort;
use std::path::PathBuf;

// ---------------------------------------------------------------------------
// Exec — external-process spawn port
// ---------------------------------------------------------------------------

/// The spawn effect: run a fully-formed [`ExecSpec`] and return its result. The
/// trait is defined here (not in `shoal-value`) because its signature is stated
/// in `shoal-exec`'s own types; `shoal-value` must stay a leaf crate.
pub trait Exec: Send + Sync {
    /// Spawn a child per `spec`, honoring `cancel`, returning the captured
    /// result — a thin wrapper over [`shoal_exec::run`].
    fn run(&self, spec: ExecSpec, cancel: &CancelToken) -> std::io::Result<ExecResult>;
}

/// The default [`Exec`] adapter: [`shoal_exec::run`] verbatim.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdExec;

impl Exec for StdExec {
    fn run(&self, spec: ExecSpec, cancel: &CancelToken) -> std::io::Result<ExecResult> {
        shoal_exec::run(spec, cancel)
    }
}

// ---------------------------------------------------------------------------
// SecretPort — StdSecret adapter over shoal-secret
// ---------------------------------------------------------------------------

/// The default [`SecretPort`] adapter: resolve the per-user secret directory
/// from the environment (`SHOAL_SECRET_DIR` / `XDG_DATA_HOME` / `HOME`) exactly
/// as `secret.get` did inline, open the [`shoal_secret::SecretStore`], and read
/// the named secret's bytes.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdSecret;

impl StdSecret {
    /// The secret directory, mirroring the original inline resolution order.
    fn dir() -> PathBuf {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        std::env::var_os("SHOAL_SECRET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::var_os("XDG_DATA_HOME")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| home.join(".local/share"))
                    .join("shoal/secrets")
            })
    }
}

impl SecretPort for StdSecret {
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        let store = shoal_secret::SecretStore::open(Self::dir()).map_err(|e| e.to_string())?;
        store
            .get(name)
            .map(|opt| opt.map(|bytes| bytes.to_vec()))
            .map_err(|e| e.to_string())
    }
}
