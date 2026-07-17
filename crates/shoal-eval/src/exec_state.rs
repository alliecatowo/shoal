//! Mutable per-evaluation execution state.

use super::*;

pub(crate) struct ShellState {
    pub(crate) env: Env,
    pub(crate) cwd: PathBuf,
    pub(crate) process_env: Vec<(OsString, OsString)>,
    pub(crate) jump_store: Option<PathBuf>,
    pub(crate) oldpwd: Option<PathBuf>,
    pub(crate) dir_stack: Vec<PathBuf>,
}

pub(crate) struct ControlState {
    pub(crate) it: Value,
    pub(crate) cancel: CancelToken,
    pub(crate) call_depth: usize,
    pub(crate) in_fn_body: usize,
    pub(crate) current_entry: Option<i64>,
    pub(crate) source: Option<String>,
    pub(crate) pending_exit: Option<i32>,
}

pub(crate) struct JobState {
    tasks: std::cell::RefCell<Vec<shoal_value::TaskVal>>,
    pub(crate) external: std::collections::HashMap<u64, u32>,
    pub(crate) pending_stop: Option<(u64, String)>,
}

/// Completed rows are useful as short job history, but the evaluator may live
/// for days and spawn an unbounded number of tasks. Active/stopped tasks are
/// never pruned; cloned `TaskVal` handles remain valid after registry pruning.
pub const MAX_COMPLETED_JOBS: usize = 256;

impl JobState {
    pub(crate) fn register(&self, task: shoal_value::TaskVal) {
        let mut tasks = self.tasks.borrow_mut();
        tasks.push(task);
        prune_completed(&mut tasks);
    }

    pub(crate) fn with_tasks<R>(&self, read: impl FnOnce(&[shoal_value::TaskVal]) -> R) -> R {
        let mut tasks = self.tasks.borrow_mut();
        prune_completed(&mut tasks);
        read(&tasks)
    }

    pub(crate) fn task(&self, id: u64) -> Option<shoal_value::TaskVal> {
        self.with_tasks(|tasks| tasks.iter().find(|task| task.id == id).cloned())
    }
}

fn prune_completed(tasks: &mut Vec<shoal_value::TaskVal>) {
    let mut excess = tasks
        .iter()
        .filter(|task| task.is_done())
        .count()
        .saturating_sub(MAX_COMPLETED_JOBS);
    if excess == 0 {
        return;
    }
    tasks.retain(|task| {
        if excess > 0 && task.is_done() {
            excess -= 1;
            false
        } else {
            true
        }
    });
}

pub(crate) struct ModuleState {
    pub(crate) cache: std::collections::HashMap<PathBuf, Value>,
    pub(crate) stack: Vec<PathBuf>,
}

pub(crate) struct PlanState {
    pub(crate) programs: Vec<Program>,
}

pub(crate) struct ExecState {
    pub(crate) reef: reef_state::ReefState,
    pub(crate) shell: ShellState,
    pub(crate) control: ControlState,
    pub(crate) jobs: JobState,
    pub(crate) modules: ModuleState,
    pub(crate) plans: PlanState,
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
            shell: ShellState {
                env: Env::root(),
                cwd,
                process_env: std::env::vars_os().collect(),
                jump_store: None,
                oldpwd: None,
                dir_stack: Vec::new(),
            },
            control: ControlState {
                it: Value::Null,
                cancel: CancelToken::new(),
                call_depth: 0,
                in_fn_body: 0,
                current_entry: None,
                source: None,
                pending_exit: None,
            },
            jobs: JobState {
                tasks: std::cell::RefCell::new(Vec::new()),
                external: std::collections::HashMap::new(),
                pending_stop: None,
            },
            modules: ModuleState {
                cache: std::collections::HashMap::new(),
                stack: Vec::new(),
            },
            plans: PlanState {
                programs: Vec::new(),
            },
        }
    }

    pub(crate) fn child_seed(&self) -> ChildExecSeed {
        ChildExecSeed {
            reef: self.reef.clone(),
            cwd: self.shell.cwd.clone(),
            env: self.shell.env.clone(),
            process_env: self.shell.process_env.clone(),
            oldpwd: self.shell.oldpwd.clone(),
            dir_stack: self.shell.dir_stack.clone(),
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
            child.shell.env = env;
        }
        child.shell.process_env = process_env;
        child.shell.oldpwd = oldpwd;
        child.shell.dir_stack = dir_stack;
        child.control.cancel = cancel;
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

    #[test]
    fn execution_state_is_partitioned_into_typed_contexts() {
        let ExecState {
            reef,
            shell,
            control,
            jobs,
            modules,
            plans,
        } = ExecState::root(PathBuf::from("/"));
        drop((reef, shell, control, jobs, modules, plans));
    }

    #[test]
    fn evaluator_does_not_deref_into_execution_state() {
        let facade = include_str!("lib.rs");
        assert!(!facade.contains("impl std::ops::Deref for Evaluator"));
        assert!(!facade.contains("impl std::ops::DerefMut for Evaluator"));
    }
}
