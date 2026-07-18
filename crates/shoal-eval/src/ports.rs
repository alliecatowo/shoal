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
use std::time::Duration;

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

    /// Run a hostile-input-safe short provider command. The default denies the
    /// capability so custom hosts cannot accidentally route provider hooks
    /// around their process adapter.
    fn run_bounded(
        &self,
        _spec: ExecSpec,
        _timeout: Duration,
        _output_cap: usize,
        _cancel: &CancelToken,
    ) -> std::io::Result<shoal_exec::BoundedCommandOutput> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "the installed execution port does not support bounded provider commands",
        ))
    }
}

/// The default [`Exec`] adapter: [`shoal_exec::run`] verbatim.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdExec;

impl Exec for StdExec {
    fn run(&self, spec: ExecSpec, cancel: &CancelToken) -> std::io::Result<ExecResult> {
        shoal_exec::run(spec, cancel)
    }

    fn run_bounded(
        &self,
        spec: ExecSpec,
        timeout: Duration,
        output_cap: usize,
        cancel: &CancelToken,
    ) -> std::io::Result<shoal_exec::BoundedCommandOutput> {
        shoal_exec::run_bounded(spec, timeout, output_cap, cancel)
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
        std::env::var_os("SHOAL_SECRET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                shoal_paths::ShoalPaths::discover()
                    .data_dir()
                    .join("secrets")
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
