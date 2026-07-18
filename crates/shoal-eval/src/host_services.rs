//! Immutable, shareable host capability bundle.

use super::*;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Native evaluator workers retained by one session (`spawn`, each `parallel`
/// lane, and each `on` handler). Children share this budget through the
/// `Arc<HostServices>` inheritance seam, so nesting cannot reset the count.
pub(crate) const MAX_SESSION_NATIVE_WORKERS: usize = 64;

/// Backstop across all evaluator sessions in this process. This is deliberately
/// higher than the per-session cap so one session fails locally first while a
/// many-session host still has a finite native-thread ceiling.
pub(crate) const MAX_PROCESS_NATIVE_WORKERS: usize = 512;

static ACTIVE_PROCESS_NATIVE_WORKERS: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
pub(crate) struct NativeWorkerBudget {
    active_session: AtomicUsize,
    session_limit: usize,
    active_process: &'static AtomicUsize,
    process_limit: usize,
}

impl Default for NativeWorkerBudget {
    fn default() -> Self {
        Self {
            active_session: AtomicUsize::new(0),
            session_limit: MAX_SESSION_NATIVE_WORKERS,
            active_process: &ACTIVE_PROCESS_NATIVE_WORKERS,
            process_limit: MAX_PROCESS_NATIVE_WORKERS,
        }
    }
}

impl NativeWorkerBudget {
    pub(crate) fn acquire(self: &Arc<Self>) -> VResult<NativeWorkerLease> {
        reserve(&self.active_session, self.session_limit).map_err(|_| {
            ErrorVal::new(
                "session_worker_limit",
                format!(
                    "session native-worker limit reached ({})",
                    self.session_limit
                ),
            )
            .with_hint(
                "await or cancel an existing spawn/parallel/on worker before starting another",
            )
        })?;

        if reserve(self.active_process, self.process_limit).is_err() {
            self.active_session.fetch_sub(1, Ordering::Relaxed);
            return Err(ErrorVal::new(
                "process_worker_limit",
                format!(
                    "process native-worker limit reached ({})",
                    self.process_limit
                ),
            )
            .with_hint("wait for another evaluator worker to finish, then retry"));
        }

        Ok(NativeWorkerLease {
            budget: Arc::clone(self),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_limits(
        session_limit: usize,
        active_process: &'static AtomicUsize,
        process_limit: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            active_session: AtomicUsize::new(0),
            session_limit,
            active_process,
            process_limit,
        })
    }
}

fn reserve(counter: &AtomicUsize, limit: usize) -> Result<(), ()> {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |active| {
            (active < limit).then_some(active + 1)
        })
        .map(|_| ())
        .map_err(|_| ())
}

/// RAII reservation moved into the worker closure before thread creation. A
/// failed `Builder::spawn` drops the closure and releases both counters; normal
/// completion and unwind release them when the worker exits.
#[derive(Debug)]
pub(crate) struct NativeWorkerLease {
    budget: Arc<NativeWorkerBudget>,
}

impl Drop for NativeWorkerLease {
    fn drop(&mut self) {
        self.budget.active_process.fetch_sub(1, Ordering::Relaxed);
        self.budget.active_session.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Clone)]
pub(crate) struct HostServices {
    pub(crate) fs: Arc<dyn Fs>,
    pub(crate) watch: Arc<dyn WatchPort>,
    pub(crate) exec: Arc<dyn Exec>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) opener: Arc<dyn Opener>,
    pub(crate) secrets: Arc<dyn SecretPort>,
    pub(crate) config: Arc<dyn ConfigPort>,
    pub(crate) adapters: AdapterCatalog,
    pub(crate) wasm: Option<Arc<WasmRegistry>>,
    pub(crate) bus: Arc<channels::EventBus>,
    pub(crate) reef_resolver: OnceLock<Arc<shoal_reef::Resolver>>,
    pub(crate) reef_user_manifest: Option<PathBuf>,
    pub(crate) native_workers: Arc<NativeWorkerBudget>,
}

impl Default for HostServices {
    fn default() -> Self {
        Self {
            fs: Arc::new(StdFs),
            watch: Arc::new(StdWatchPort),
            exec: Arc::new(StdExec),
            clock: Arc::new(StdClock),
            opener: Arc::new(StdOpener),
            secrets: Arc::new(StdSecret),
            config: Arc::new(ConfigSnapshot::default()),
            adapters: AdapterCatalog::empty(),
            wasm: None,
            bus: channels::EventBus::shared(),
            reef_resolver: OnceLock::new(),
            reef_user_manifest: None,
            native_workers: Arc::new(NativeWorkerBudget::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_budget_rejects_then_reclaims_without_poisonable_state() {
        static PROCESS: AtomicUsize = AtomicUsize::new(0);
        let budget = NativeWorkerBudget::with_limits(2, &PROCESS, 3);

        let first = budget.acquire().unwrap();
        let second = budget.acquire().unwrap();
        let error = budget.acquire().unwrap_err();
        assert_eq!(error.code, "session_worker_limit");

        drop(first);
        let replacement = budget.acquire().unwrap();
        drop(second);
        drop(replacement);
        assert_eq!(budget.active_session.load(Ordering::Relaxed), 0);
        assert_eq!(PROCESS.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn child_evaluators_share_the_parent_session_budget() {
        static PROCESS: AtomicUsize = AtomicUsize::new(0);
        let budget = NativeWorkerBudget::with_limits(1, &PROCESS, 4);
        let mut parent = Evaluator::new(PathBuf::from("/"));
        Arc::make_mut(&mut parent.host).native_workers = budget.clone();
        let child = parent
            .child_context()
            .build(ChildKind::Spawn, CancelToken::new());
        assert!(Arc::ptr_eq(
            &parent.host.native_workers,
            &child.host.native_workers
        ));

        let lease = parent.host.native_workers.acquire().unwrap();
        let error = child.host.native_workers.acquire().unwrap_err();
        assert_eq!(error.code, "session_worker_limit");
        drop(lease);
        assert!(child.host.native_workers.acquire().is_ok());
    }

    #[test]
    fn process_rejection_rolls_back_the_session_reservation() {
        static PROCESS: AtomicUsize = AtomicUsize::new(0);
        let blocker = NativeWorkerBudget::with_limits(2, &PROCESS, 1);
        let other_session = NativeWorkerBudget::with_limits(1, &PROCESS, 1);
        let process_lease = blocker.acquire().unwrap();

        let error = other_session.acquire().unwrap_err();
        assert_eq!(error.code, "process_worker_limit");
        assert_eq!(
            other_session.active_session.load(Ordering::Relaxed),
            0,
            "failed process admission must roll back its session slot"
        );
        drop(process_lease);
    }
}
