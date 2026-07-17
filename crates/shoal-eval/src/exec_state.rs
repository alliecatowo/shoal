//! Mutable per-evaluation execution state.

use super::*;

pub struct ExecState {
    pub(crate) reef: reef_state::ReefState,
    /// Public for source compatibility; the enclosing Evaluator dereferences to
    /// ExecState. Child construction still controls whether this handle is
    /// inherited or replaced with a fresh root.
    pub env: Env,
    pub(crate) cwd: PathBuf,
    pub(crate) process_env: Vec<(OsString, OsString)>,
    pub it: Value,
    pub(crate) cancel: CancelToken,
    pub(crate) call_depth: usize,
    pub(crate) in_fn_body: usize,
    pub(crate) jobs: Vec<shoal_value::TaskVal>,
    pub(crate) external_jobs: std::collections::HashMap<u64, u32>,
    pub(crate) pending_stop: Option<(u64, String)>,
    pub(crate) current_entry: Option<i64>,
    pub(crate) source: Option<String>,
    pub(crate) pending_exit: Option<i32>,
    pub(crate) modules: std::collections::HashMap<PathBuf, Value>,
    pub(crate) module_stack: Vec<PathBuf>,
    pub(crate) plans: Vec<Program>,
    pub(crate) jump_store: Option<PathBuf>,
    pub(crate) oldpwd: Option<PathBuf>,
    pub(crate) dir_stack: Vec<PathBuf>,
}

/// The complete mutable snapshot allowed to cross a parent→child boundary.
/// Fresh-only fields are absent by construction.
pub(crate) struct ChildExecSeed {
    reef: reef_state::ReefState,
    cwd: PathBuf,
    env: Env,
    process_env: Vec<(OsString, OsString)>,
    oldpwd: Option<PathBuf>,
    dir_stack: Vec<PathBuf>,
}

impl ExecState {
    pub(crate) fn root(cwd: PathBuf) -> Self {
        Self {
            reef: reef_state::ReefState::default(),
            env: Env::root(),
            cwd,
            process_env: std::env::vars_os().collect(),
            it: Value::Null,
            cancel: CancelToken::new(),
            call_depth: 0,
            in_fn_body: 0,
            jobs: Vec::new(),
            external_jobs: std::collections::HashMap::new(),
            pending_stop: None,
            current_entry: None,
            source: None,
            pending_exit: None,
            modules: std::collections::HashMap::new(),
            module_stack: Vec::new(),
            plans: Vec::new(),
            jump_store: None,
            oldpwd: None,
            dir_stack: Vec::new(),
        }
    }

    pub(crate) fn child_seed(&self) -> ChildExecSeed {
        ChildExecSeed {
            reef: self.reef.clone(),
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            process_env: self.process_env.clone(),
            oldpwd: self.oldpwd.clone(),
            dir_stack: self.dir_stack.clone(),
        }
    }

    pub(crate) fn child(seed: ChildExecSeed, kind: crate::ChildKind, cancel: CancelToken) -> Self {
        let ChildExecSeed {
            reef,
            cwd,
            env,
            process_env,
            oldpwd,
            dir_stack,
        } = seed;
        let mut child = Self::root(cwd);
        child.reef = reef;
        if !matches!(kind, crate::ChildKind::Script) {
            child.env = env;
        }
        child.process_env = process_env;
        child.oldpwd = oldpwd;
        child.dir_stack = dir_stack;
        child.cancel = cancel;
        child
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evaluator_facade_has_exactly_three_owned_contexts() {
        let evaluator = Evaluator::new(PathBuf::from("/"));
        let Evaluator {
            host,
            session,
            exec,
        } = evaluator;
        drop((host, session, exec));
    }
}
