//! Interactive kernel-protocol routing and authenticated state projection.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use shoal_value::Env;

use super::{parse_job_control, rewrite_fg};
use crate::kernel_repl::{KernelRpc, ProtocolOutcome, ProtocolSession};
use crate::repl_state::{ProtocolSnapshot, RemoteEnvMirror};

pub(super) trait ReplProtocol {
    fn execute(
        &mut self,
        src: &str,
        interrupt: &AtomicBool,
        width: usize,
    ) -> Result<ProtocolOutcome, String>;
    fn snapshot(&mut self) -> Result<serde_json::Value, String>;
}

impl<R: KernelRpc> ReplProtocol for ProtocolSession<R> {
    fn execute(
        &mut self,
        src: &str,
        interrupt: &AtomicBool,
        width: usize,
    ) -> Result<ProtocolOutcome, String> {
        ProtocolSession::execute(self, src, interrupt, width)
    }

    fn snapshot(&mut self) -> Result<serde_json::Value, String> {
        ProtocolSession::snapshot(self)
    }
}

pub(super) fn protocol_requested(standalone: bool, kernel_enabled: bool) -> bool {
    !standalone && kernel_enabled
}

pub(super) fn execute_protocol_line(
    session: &mut impl ReplProtocol,
    src: &str,
    interrupt: &AtomicBool,
    width: usize,
) -> Result<ProtocolOutcome, String> {
    if parse_job_control(src).is_some() {
        return Err("fg/bg process-group job control is available only with --standalone".into());
    }
    let run_src = rewrite_fg(src).unwrap_or_else(|| src.to_string());
    session.execute(&run_src, interrupt, width)
}

pub(super) fn refresh_protocol_state(
    session: &mut impl ReplProtocol,
    mirror: &mut RemoteEnvMirror,
    env: &Env,
    cwd: &Arc<Mutex<PathBuf>>,
    path_dirs: &Arc<Mutex<Option<Vec<PathBuf>>>>,
) -> Result<ProtocolSnapshot, String> {
    let snapshot = ProtocolSnapshot::parse(session.snapshot()?)?;
    mirror.apply(&snapshot, env, cwd, path_dirs);
    Ok(snapshot)
}

pub(super) fn session_path_dirs(env_vars: &[(OsString, OsString)], cwd: &Path) -> Vec<PathBuf> {
    let Some((_, path)) = env_vars
        .iter()
        .rev()
        .find(|(name, _)| name == OsStr::new("PATH"))
    else {
        return Vec::new();
    };
    std::env::split_paths(path)
        .map(|dir| {
            if dir.is_absolute() {
                dir
            } else {
                cwd.join(dir)
            }
        })
        .take(256)
        .collect()
}
