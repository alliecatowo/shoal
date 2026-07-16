//! `TaskVal`/`TaskShared` — task handles and job control (TDD §4.7), moved
//! verbatim out of `lib.rs`.

use super::*;
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

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
    on_cancel: Mutex<Vec<Box<dyn Fn() + Send>>>,
    /// Suspend state (TDD §4.7 job control): `task.suspend()` SIGTSTPs the task's
    /// process group, `task.resume()` SIGCONTs it. The actual OS signal is sent
    /// by hooks a spawner/host registers (`on_suspend`/`on_resume`), so this
    /// mechanism is signalling-backend-agnostic (a thread-only task simply has no
    /// hooks). `suspended` tracks the flag for `jobs`/prompt accounting.
    suspended: AtomicBool,
    on_suspend: Mutex<Vec<Box<dyn Fn() + Send>>>,
    on_resume: Mutex<Vec<Box<dyn Fn() + Send>>>,
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
        let mut g = self.shared.state.lock().unwrap();
        *g = TaskState::Done(result);
        self.shared.cond.notify_all();
    }

    pub fn wait(&self) -> VResult<Value> {
        let mut g = self.shared.state.lock().unwrap();
        loop {
            match &*g {
                TaskState::Done(r) => return r.clone(),
                TaskState::Running => g = self.shared.cond.wait(g).unwrap(),
            }
        }
    }

    pub fn is_done(&self) -> bool {
        matches!(&*self.shared.state.lock().unwrap(), TaskState::Done(_))
    }

    pub fn cancel(&self) {
        self.shared.cancel_requested.store(true, Ordering::SeqCst);
        for hook in self.shared.on_cancel.lock().unwrap().iter() {
            hook();
        }
    }

    pub fn cancel_requested(&self) -> bool {
        self.shared.cancel_requested.load(Ordering::SeqCst)
    }

    pub fn on_cancel(&self, hook: Box<dyn Fn() + Send>) {
        if self.cancel_requested() {
            hook();
        } else {
            self.shared.on_cancel.lock().unwrap().push(hook);
        }
    }

    /// Request the task suspend (TDD §4.7): mark it suspended and run every
    /// registered suspend hook (which is where a spawner/host sends `SIGTSTP` to
    /// the task's process group). Idempotent — suspending an already-suspended
    /// task re-runs the hooks, which is harmless (`SIGTSTP` to a stopped group is
    /// a no-op).
    pub fn suspend(&self) {
        self.shared.suspended.store(true, Ordering::SeqCst);
        for hook in self.shared.on_suspend.lock().unwrap().iter() {
            hook();
        }
    }

    /// Request the task resume (TDD §4.7): clear the suspended flag and run every
    /// registered resume hook (`SIGCONT` to the process group). Idempotent.
    pub fn resume(&self) {
        self.shared.suspended.store(false, Ordering::SeqCst);
        for hook in self.shared.on_resume.lock().unwrap().iter() {
            hook();
        }
    }

    pub fn is_suspended(&self) -> bool {
        self.shared.suspended.load(Ordering::SeqCst)
    }

    /// Mark the task suspended WITHOUT running the suspend hooks. Used when the
    /// OS already stopped the underlying process — a foreground external command
    /// SIGTSTP'd by Ctrl-Z (TDD §4.7 job control): the stop physically happened,
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
        if self.is_suspended() {
            hook();
        }
        self.shared.on_suspend.lock().unwrap().push(hook);
    }

    /// Register a hook run when the task is resumed (`SIGCONT`).
    pub fn on_resume(&self, hook: Box<dyn Fn() + Send>) {
        self.shared.on_resume.lock().unwrap().push(hook);
    }

    pub fn same(&self, other: &TaskVal) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}
