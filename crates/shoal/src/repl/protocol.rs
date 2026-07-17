//! Interactive kernel-protocol routing and authenticated state projection.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use shoal_eval::Evaluator;
use shoal_value::Env;

use super::{parse_job_control, rewrite_fg};
use crate::embedded_kernel::{EmbeddedKernelChild, EmbeddedKernelConfig};
use crate::kernel_repl::{KernelRpc, ProtocolOutcome, ProtocolSession};
use crate::repl_state::{ProtocolSnapshot, RemoteEnvMirror};

/// Kernel-backed or standalone protocol projection. It owns only transport,
/// authenticated snapshots, and the two editor cells derived from them.
pub(super) struct ProtocolState {
    session: Option<ProtocolSession<shoal_mcp::KernelClient>>,
    child: Option<EmbeddedKernelChild>,
    snapshot: Option<ProtocolSnapshot>,
    mirror: RemoteEnvMirror,
    interrupt: Arc<AtomicBool>,
    cwd: Arc<Mutex<PathBuf>>,
    path_dirs: Arc<Mutex<Option<Vec<PathBuf>>>>,
}

impl ProtocolState {
    pub(super) fn connect(
        enabled: bool,
        config: &shoal_config::Config,
        state_dir: PathBuf,
        cwd: PathBuf,
    ) -> Result<Self, String> {
        let (session, child) = if enabled {
            let (client, child) = crate::embedded_kernel::connect(EmbeddedKernelConfig {
                session: config.kernel.session.clone(),
                state_dir,
                policy: config.leash.policy.clone(),
                program: None,
            })?;
            (Some(ProtocolSession::new(client)), Some(child))
        } else {
            (None, None)
        };
        Ok(Self {
            session,
            child,
            snapshot: None,
            mirror: RemoteEnvMirror::default(),
            interrupt: Arc::new(AtomicBool::new(false)),
            cwd: Arc::new(Mutex::new(cwd)),
            path_dirs: Arc::new(Mutex::new(None)),
        })
    }

    pub(super) fn is_backed(&self) -> bool {
        self.session.is_some()
    }

    pub(super) fn interrupt_handle(&self) -> Arc<AtomicBool> {
        self.interrupt.clone()
    }

    pub(super) fn cwd_cell(&self) -> Arc<Mutex<PathBuf>> {
        self.cwd.clone()
    }

    pub(super) fn path_dirs_cell(&self) -> Arc<Mutex<Option<Vec<PathBuf>>>> {
        self.path_dirs.clone()
    }

    pub(super) fn snapshot(&self) -> Option<&ProtocolSnapshot> {
        self.snapshot.as_ref()
    }

    pub(super) fn refresh(&mut self, evaluator: &Evaluator) -> Result<(), String> {
        if let Some(session) = self.session.as_mut() {
            self.snapshot = Some(refresh_protocol_state(
                session,
                &mut self.mirror,
                evaluator.env(),
                &self.cwd,
                &self.path_dirs,
            )?);
        } else {
            if let Ok(mut cwd) = self.cwd.lock() {
                *cwd = evaluator.cwd().to_path_buf();
            }
            if let Ok(mut paths) = self.path_dirs.lock() {
                *paths = Some(session_path_dirs(evaluator.env_vars(), evaluator.cwd()));
            }
        }
        Ok(())
    }

    pub(super) fn execute(&mut self, src: &str, width: usize) -> Result<ProtocolOutcome, String> {
        let session = self
            .session
            .as_mut()
            .ok_or_else(|| "interactive protocol session is unavailable".to_string())?;
        execute_protocol_line(session, src, &self.interrupt, width)
    }

    pub(super) fn reset_interrupt(&self) {
        self.interrupt.store(false, Ordering::SeqCst);
    }

    pub(super) fn shutdown(&mut self) {
        drop(self.child.take());
    }
}

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
