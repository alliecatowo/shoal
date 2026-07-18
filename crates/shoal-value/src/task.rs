//! `TaskVal`/`TaskShared` — task handles and job control (site/content/internals/language-conformance-contract.md), moved
//! verbatim out of `lib.rs`.

use super::*;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Condvar, MutexGuard};

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

type TaskHook = Arc<Mutex<Box<dyn Fn() + Send>>>;

const TASK_STATE_POISONED_CODE: &str = "custom";
const TASK_STATE_POISONED_MSG: &str = "task state is unavailable after a synchronization failure";

#[derive(Debug, Clone)]
pub struct TaskVal {
    pub id: u64,
    pub shared: Arc<TaskShared>,
}

pub struct TaskShared {
    pub desc: String,
    state: Mutex<TaskState>,
    cond: Condvar,
    cancel_requested: AtomicBool,
    /// Hooks run on cancel (e.g. cancel exec tokens of children).
    on_cancel: Mutex<Vec<TaskHook>>,
    /// Suspend state (site/content/internals/language-conformance-contract.md job control): `task.suspend()` SIGTSTPs the task's
    /// process group, `task.resume()` SIGCONTs it. The actual OS signal is sent
    /// by hooks a spawner/host registers (`on_suspend`/`on_resume`), so this
    /// mechanism is signalling-backend-agnostic (a thread-only task simply has no
    /// hooks). `suspended` tracks the flag for `jobs`/prompt accounting.
    suspended: AtomicBool,
    on_suspend: Mutex<Vec<TaskHook>>,
    on_resume: Mutex<Vec<TaskHook>>,
}

impl std::fmt::Debug for TaskShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TaskShared({})", self.desc)
    }
}

enum TaskState {
    Running,
    Done(VResult<Value>),
}

fn task_state_poisoned() -> ErrorVal {
    ErrorVal::new(TASK_STATE_POISONED_CODE, TASK_STATE_POISONED_MSG)
}

/// Run a registry snapshot without holding either the registry mutex or a
/// mutex that a hook panic can poison. A failed hook is isolated from later
/// hooks; a hook whose own mutex is already poisoned is never re-entered.
fn run_hooks(hooks: Vec<TaskHook>) {
    for hook in hooks {
        // Recursive/concurrent lifecycle requests must not deadlock on a hook
        // already in flight. That invocation owns delivery; this one skips it.
        let Ok(hook) = hook.try_lock() else {
            continue;
        };
        let _ = catch_unwind(AssertUnwindSafe(&**hook));
    }
}

impl TaskShared {
    /// A poisoned task state has unknown invariants. Repair it to one stable,
    /// terminal language error so current/future waiters wake instead of
    /// hanging and `finish` cannot overwrite the failure.
    fn repair_state_poison<'a>(
        &'a self,
        mut state: MutexGuard<'a, TaskState>,
    ) -> MutexGuard<'a, TaskState> {
        *state = TaskState::Done(Err(task_state_poisoned()));
        self.state.clear_poison();
        self.cond.notify_all();
        state
    }

    fn lock_state(&self) -> MutexGuard<'_, TaskState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => self.repair_state_poison(poisoned.into_inner()),
        }
    }
}

impl TaskVal {
    pub fn new(desc: impl Into<String>) -> TaskVal {
        TaskVal {
            id: NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed),
            shared: Arc::new(TaskShared {
                desc: desc.into(),
                state: Mutex::new(TaskState::Running),
                cond: Condvar::new(),
                cancel_requested: AtomicBool::new(false),
                on_cancel: Mutex::new(Vec::new()),
                suspended: AtomicBool::new(false),
                on_suspend: Mutex::new(Vec::new()),
                on_resume: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn finish(&self, result: VResult<Value>) {
        let mut state = self.shared.lock_state();
        if matches!(*state, TaskState::Running) {
            *state = TaskState::Done(result);
            self.shared.cond.notify_all();
        }
    }

    pub fn wait(&self) -> VResult<Value> {
        let mut state = self.shared.lock_state();
        loop {
            match &*state {
                TaskState::Done(r) => return r.clone(),
                TaskState::Running => {
                    state = match self.shared.cond.wait(state) {
                        Ok(state) => state,
                        Err(poisoned) => self.shared.repair_state_poison(poisoned.into_inner()),
                    };
                }
            }
        }
    }

    /// Whether the task has reached a terminal state. Poison is conservatively
    /// repaired as a terminal error, so this returns `true` rather than leaving
    /// callers believing an unknowable task is still making progress.
    pub fn is_done(&self) -> bool {
        matches!(&*self.shared.lock_state(), TaskState::Done(_))
    }

    pub fn cancel(&self) {
        let hooks = match self.shared.on_cancel.lock() {
            Ok(hooks) => {
                self.shared.cancel_requested.store(true, Ordering::SeqCst);
                hooks.clone()
            }
            Err(poisoned) => {
                poisoned.into_inner().clear();
                self.shared.on_cancel.clear_poison();
                self.shared.cancel_requested.store(true, Ordering::SeqCst);
                Vec::new()
            }
        };
        run_hooks(hooks);
    }

    pub fn cancel_requested(&self) -> bool {
        self.shared.cancel_requested.load(Ordering::SeqCst)
    }

    pub fn on_cancel(&self, hook: Box<dyn Fn() + Send>) {
        let hook = Arc::new(Mutex::new(hook));
        let run_now = match self.shared.on_cancel.lock() {
            Ok(mut hooks) => {
                if self.cancel_requested() {
                    true
                } else {
                    hooks.push(hook.clone());
                    false
                }
            }
            Err(poisoned) => {
                poisoned.into_inner().clear();
                self.shared.on_cancel.clear_poison();
                return;
            }
        };
        if run_now {
            run_hooks(vec![hook]);
        }
    }

    /// Request the task suspend (site/content/internals/language-conformance-contract.md): mark it suspended and run every
    /// registered suspend hook (which is where a spawner/host sends `SIGTSTP` to
    /// the task's process group). Idempotent — suspending an already-suspended
    /// task re-runs the hooks, which is harmless (`SIGTSTP` to a stopped group is
    /// a no-op).
    pub fn suspend(&self) {
        let hooks = match self.shared.on_suspend.lock() {
            Ok(hooks) => {
                self.shared.suspended.store(true, Ordering::SeqCst);
                hooks.clone()
            }
            Err(poisoned) => {
                poisoned.into_inner().clear();
                self.shared.on_suspend.clear_poison();
                self.shared.suspended.store(true, Ordering::SeqCst);
                Vec::new()
            }
        };
        run_hooks(hooks);
    }

    /// Request the task resume (site/content/internals/language-conformance-contract.md): clear the suspended flag and run every
    /// registered resume hook (`SIGCONT` to the process group). Idempotent.
    pub fn resume(&self) {
        let hooks = match self.shared.on_resume.lock() {
            Ok(hooks) => {
                self.shared.suspended.store(false, Ordering::SeqCst);
                hooks.clone()
            }
            Err(poisoned) => {
                poisoned.into_inner().clear();
                self.shared.on_resume.clear_poison();
                self.shared.suspended.store(false, Ordering::SeqCst);
                Vec::new()
            }
        };
        run_hooks(hooks);
    }

    pub fn is_suspended(&self) -> bool {
        self.shared.suspended.load(Ordering::SeqCst)
    }

    /// Mark the task suspended WITHOUT running the suspend hooks. Used when the
    /// OS already stopped the underlying process — a foreground external command
    /// SIGTSTP'd by Ctrl-Z (site/content/internals/language-conformance-contract.md job control): the stop physically happened,
    /// so firing the `on_suspend` hooks (which re-send `SIGTSTP`) would be
    /// redundant. `jobs`/prompt accounting then reflects the stop. Contrast with
    /// [`TaskVal::suspend`], which is the request-a-suspend path that DOES signal.
    pub fn mark_suspended(&self) {
        self.shared.suspended.store(true, Ordering::SeqCst);
    }

    /// Clear the suspended flag WITHOUT running the resume hooks — for a caller
    /// that performs the `SIGCONT` + terminal handoff itself (the REPL `fg`/`bg`
    /// job-control path drives the terminal re-attach directly rather than via a
    /// generic hook). Counterpart to [`TaskVal::mark_suspended`].
    pub fn mark_resumed(&self) {
        self.shared.suspended.store(false, Ordering::SeqCst);
    }

    /// Register a hook run when the task is suspended (e.g. `SIGTSTP` the child
    /// process group). If the task is already suspended, the hook fires now.
    pub fn on_suspend(&self, hook: Box<dyn Fn() + Send>) {
        let hook = Arc::new(Mutex::new(hook));
        let run_now = match self.shared.on_suspend.lock() {
            Ok(mut hooks) => {
                hooks.push(hook.clone());
                self.is_suspended()
            }
            Err(poisoned) => {
                poisoned.into_inner().clear();
                self.shared.on_suspend.clear_poison();
                return;
            }
        };
        if run_now {
            run_hooks(vec![hook]);
        }
    }

    /// Register a hook run when the task is resumed (`SIGCONT`).
    pub fn on_resume(&self, hook: Box<dyn Fn() + Send>) {
        let hook = Arc::new(Mutex::new(hook));
        match self.shared.on_resume.lock() {
            Ok(mut hooks) => hooks.push(hook),
            Err(poisoned) => {
                poisoned.into_inner().clear();
                self.shared.on_resume.clear_poison();
            }
        }
    }

    pub fn same(&self, other: &TaskVal) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    fn poison<T>(mutex: &Mutex<T>) {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _guard = mutex.lock().expect("test mutex starts healthy");
            panic!("inject mutex poison");
        }));
        assert!(mutex.is_poisoned());
    }

    fn poison_registry_with_hook(registry: &Mutex<Vec<TaskHook>>, hook: TaskHook) {
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let mut hooks = registry.lock().expect("test registry starts healthy");
            hooks.push(hook);
            panic!("inject registry poison");
        }));
        assert!(registry.is_poisoned());
    }

    #[test]
    fn poisoned_state_wakes_waiters_with_a_stable_terminal_error() {
        let task = TaskVal::new("poisoned state");
        let waiter = task.clone();
        let thread = thread::spawn(move || waiter.wait());

        poison(&task.shared.state);
        // The first ordinary lifecycle caller repairs poison and must wake a
        // waiter without a test-only Condvar notification.
        task.finish(Ok(Value::Int(42)));

        let expected = task_state_poisoned();
        assert_eq!(
            thread.join().expect("waiter must not panic"),
            Err(expected.clone())
        );
        assert_eq!(task.wait(), Err(expected.clone()));
        assert!(task.is_done());

        task.finish(Err(ErrorVal::new("custom", "late failure")));
        assert_eq!(task.wait(), Err(expected));
        assert!(!task.shared.state.is_poisoned());
    }

    #[test]
    fn finish_is_idempotent_for_healthy_tasks() {
        let task = TaskVal::new("finished");
        task.finish(Ok(Value::Int(1)));
        task.finish(Ok(Value::Int(2)));
        assert_eq!(task.wait(), Ok(Value::Int(1)));
    }

    #[test]
    fn poisoned_cancel_registry_is_discarded_and_stays_non_panicking() {
        let task = TaskVal::new("cancel registry");
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_for_hook = calls.clone();
        poison_registry_with_hook(
            &task.shared.on_cancel,
            Arc::new(Mutex::new(Box::new(move || {
                calls_for_hook.fetch_add(1, Ordering::SeqCst);
            }))),
        );

        task.cancel();
        task.cancel();
        assert!(task.cancel_requested());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(!task.shared.on_cancel.is_poisoned());

        let known_calls = calls.clone();
        task.on_cancel(Box::new(move || {
            known_calls.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn poisoned_job_control_registries_are_discarded_and_reusable() {
        let task = TaskVal::new("job control registries");
        let calls = Arc::new(AtomicUsize::new(0));
        for registry in [&task.shared.on_suspend, &task.shared.on_resume] {
            let calls = calls.clone();
            poison_registry_with_hook(
                registry,
                Arc::new(Mutex::new(Box::new(move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                }))),
            );
        }

        task.suspend();
        task.resume();
        task.suspend();
        task.resume();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(!task.is_suspended());
        assert!(!task.shared.on_suspend.is_poisoned());
        assert!(!task.shared.on_resume.is_poisoned());

        let suspend_calls = calls.clone();
        task.on_suspend(Box::new(move || {
            suspend_calls.fetch_add(1, Ordering::SeqCst);
        }));
        task.suspend();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn panicking_hook_is_isolated_from_registry_and_later_hooks() {
        let task = TaskVal::new("panicking hook");
        let calls = Arc::new(AtomicUsize::new(0));
        task.on_suspend(Box::new(|| panic!("hook panic")));
        let later_calls = calls.clone();
        task.on_suspend(Box::new(move || {
            later_calls.fetch_add(1, Ordering::SeqCst);
        }));

        task.suspend();
        task.suspend();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(!task.shared.on_suspend.is_poisoned());
    }

    #[test]
    fn reentrant_hook_does_not_deadlock_the_task() {
        let task = TaskVal::new("reentrant hook");
        let nested = task.clone();
        task.on_suspend(Box::new(move || nested.suspend()));

        task.suspend();
        assert!(task.is_suspended());
    }
}
