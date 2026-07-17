use super::super::{Kernel, OwnerKey, Session, now_ns};
use serde_json::{Value as Json, json};
use shoal_proto::error_code::{INTERNAL_ERROR, UNKNOWN_TASK};
use shoal_proto::{Ref, RpcError, TaskRecord};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};

pub(crate) struct TaskEntry {
    pub(crate) task: Ref,
    pub(crate) owner: OwnerKey,
    pub(crate) session_id: String,
    /// Held only while a worker can still use the evaluator session.
    pub(crate) session_lease: Mutex<Option<Arc<Session>>>,
    pub(crate) started_ns: i64,
    pub(crate) inner: Mutex<TaskInner>,
    pub(crate) done: Condvar,
    pub(crate) cancel: shoal_exec::CancelToken,
    pub(crate) cancel_requested: AtomicBool,
}

pub(crate) struct TaskInner {
    pub(crate) state: &'static str,
    pub(crate) finished_ns: Option<i64>,
    pub(crate) result_ref: Option<Ref>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) error: Option<RpcError>,
    pub(crate) active_slot: Option<QuotaPermit>,
}

impl TaskEntry {
    pub(crate) fn release_session_lease(&self) {
        let mut lease = match self.session_lease.lock() {
            Ok(lease) => lease,
            Err(poisoned) => poisoned.into_inner(),
        };
        lease.take();
        self.session_lease.clear_poison();
    }

    fn invariant_error(&self) -> RpcError {
        RpcError {
            code: INTERNAL_ERROR,
            message: "task state was reconstructed after an internal failure".into(),
            data: Some(json!({"task": self.task, "task_reconstructed": true})),
        }
    }

    fn reconstruct_locked(
        &self,
        mut inner: std::sync::MutexGuard<'_, TaskInner>,
        message: &'static str,
    ) -> Option<QuotaPermit> {
        inner.state = "failed";
        inner.finished_ns = Some(now_ns());
        inner.result_ref = None;
        inner.exit_code = None;
        inner.error = Some(RpcError {
            code: INTERNAL_ERROR,
            message: message.into(),
            data: Some(json!({"task": self.task, "task_reconstructed": true})),
        });
        inner.active_slot.take()
    }

    fn finish_reconstruction(&self, active_slot: Option<QuotaPermit>) {
        drop(active_slot);
        self.release_session_lease();
        self.done.notify_all();
    }

    pub(crate) fn lock_inner(&self) -> Result<std::sync::MutexGuard<'_, TaskInner>, RpcError> {
        match self.inner.lock() {
            Ok(inner) => Ok(inner),
            Err(poisoned) => {
                let inner = poisoned.into_inner();
                let active_slot = self.reconstruct_locked(inner, "task state mutex was poisoned");
                self.inner.clear_poison();
                self.finish_reconstruction(active_slot);
                Err(self.invariant_error())
            }
        }
    }

    pub(crate) fn repair_wait_poison(
        &self,
        poisoned: std::sync::PoisonError<std::sync::MutexGuard<'_, TaskInner>>,
    ) -> RpcError {
        let inner = poisoned.into_inner();
        let active_slot = self.reconstruct_locked(inner, "task waiter state was poisoned");
        self.inner.clear_poison();
        self.finish_reconstruction(active_slot);
        self.invariant_error()
    }

    pub(crate) fn repair_timeout_wait_poison(
        &self,
        poisoned: std::sync::PoisonError<(
            std::sync::MutexGuard<'_, TaskInner>,
            std::sync::WaitTimeoutResult,
        )>,
    ) -> RpcError {
        let (inner, _) = poisoned.into_inner();
        let active_slot = self.reconstruct_locked(inner, "task waiter state was poisoned");
        self.inner.clear_poison();
        self.finish_reconstruction(active_slot);
        self.invariant_error()
    }

    /// Restore the complete terminal-task invariant after a worker panic.
    pub(crate) fn fail_worker_panic(&self) {
        let (active_slot, notify) = {
            let inner = match self.inner.lock() {
                Ok(inner) => inner,
                Err(poisoned) => poisoned.into_inner(),
            };
            if inner.finished_ns.is_some() {
                let mut inner = inner;
                let active_slot = inner.active_slot.take();
                self.inner.clear_poison();
                (active_slot, false)
            } else {
                let active_slot = self.reconstruct_locked(inner, "task worker panicked");
                self.inner.clear_poison();
                (active_slot, true)
            }
        };
        drop(active_slot);
        self.release_session_lease();
        if notify {
            self.done.notify_all();
        }
    }
}

/// Unwind guard that prevents worker panics from stranding task state, waiters,
/// session leases, or active quota permits.
pub(crate) struct TaskWorkerGuard {
    task: Arc<TaskEntry>,
    kernel: Arc<Kernel>,
    channel: String,
    armed: bool,
}

impl TaskWorkerGuard {
    pub(crate) fn new(task: Arc<TaskEntry>, kernel: Arc<Kernel>, channel: String) -> Self {
        Self {
            task,
            kernel,
            channel,
            armed: true,
        }
    }

    pub(crate) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TaskWorkerGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.task.fail_worker_panic();
            self.kernel.events.publish(
                &self.task.owner,
                &self.channel,
                json!({
                    "$": "record",
                    "v": {
                        "state": {"$":"str", "v":"failed"},
                        "ref": Json::Null,
                    }
                }),
            );
            self.kernel.reap_finished_tasks(&self.task.owner);
        }));
    }
}

#[derive(Default)]
pub(crate) struct SessionQuota {
    pub(crate) counts: Mutex<HashMap<OwnerKey, usize>>,
    quarantined: AtomicBool,
}

pub(crate) struct QuotaPermit {
    quota: Arc<SessionQuota>,
    owner: OwnerKey,
}

impl SessionQuota {
    pub(crate) fn reserve(
        self: &Arc<Self>,
        owner: &OwnerKey,
        max: usize,
        limit: &'static str,
        noun: &'static str,
    ) -> Result<QuotaPermit, RpcError> {
        if self.quarantined.load(Ordering::Acquire) || self.counts.is_poisoned() {
            self.quarantined.store(true, Ordering::Release);
            return Err(task_quota_unavailable());
        }
        let mut counts = match self.counts.lock() {
            Ok(counts) => counts,
            Err(poisoned) => {
                drop(poisoned);
                self.quarantined.store(true, Ordering::Release);
                return Err(task_quota_unavailable());
            }
        };
        let current = counts.entry(owner.clone()).or_default();
        if *current >= max {
            return Err(RpcError {
                code: shoal_proto::error_code::QUOTA_EXCEEDED,
                message: format!("session has reached the {max}-{noun} limit"),
                data: Some(json!({"limit": limit, "max": max})),
            });
        }
        *current += 1;
        Ok(QuotaPermit {
            quota: self.clone(),
            owner: owner.clone(),
        })
    }
}

impl Drop for QuotaPermit {
    fn drop(&mut self) {
        if self.quota.quarantined.load(Ordering::Acquire) {
            return;
        }
        let mut counts = match self.quota.counts.lock() {
            Ok(counts) => counts,
            Err(poisoned) => {
                drop(poisoned);
                self.quota.quarantined.store(true, Ordering::Release);
                return;
            }
        };
        if let Some(current) = counts.get_mut(&self.owner) {
            *current = current.saturating_sub(1);
            if *current == 0 {
                counts.remove(&self.owner);
            }
        }
    }
}

fn task_quota_unavailable() -> RpcError {
    RpcError {
        code: INTERNAL_ERROR,
        message: "task quota is quarantined; restart the kernel".into(),
        data: Some(json!({
            "subsystem": "task_quota",
            "quarantined": true,
            "restart_required": true,
        })),
    }
}

pub(crate) fn task_record(task: &Arc<TaskEntry>) -> Result<TaskRecord, RpcError> {
    let inner = match task.lock_inner() {
        Ok(inner) => inner,
        // The first failure reconstructs and unpoisons the terminal record.
        Err(_) => task.lock_inner()?,
    };
    Ok(task_record_locked(task, &inner))
}

pub(crate) fn task_record_locked(task: &TaskEntry, inner: &TaskInner) -> TaskRecord {
    TaskRecord {
        task: task.task.clone(),
        session: task.session_id.clone(),
        state: inner.state.into(),
        started_ns: task.started_ns,
        finished_ns: inner.finished_ns,
        result_ref: inner.result_ref.clone(),
        exit_code: inner.exit_code,
        error: inner.error.clone(),
    }
}

const MAX_FINISHED_PER_OWNER: usize = 512;
pub(crate) const RETENTION_NS: i64 = 24 * 60 * 60 * 1_000_000_000;

/// Owns task identity, admission, lookup, and terminal retention. Registry
/// operations return cloned entries/snapshots and never expose a map guard.
pub(crate) struct TaskRegistry {
    entries: Mutex<HashMap<Ref, Arc<TaskEntry>>>,
    slots: Arc<SessionQuota>,
    max_active_per_owner: AtomicUsize,
    next_id: AtomicU64,
    quarantined: AtomicBool,
}

impl TaskRegistry {
    pub(crate) fn new(max_active_per_owner: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            slots: Arc::new(SessionQuota::default()),
            max_active_per_owner: AtomicUsize::new(max_active_per_owner),
            next_id: AtomicU64::new(1),
            quarantined: AtomicBool::new(false),
        }
    }

    pub(crate) fn configure(&self, max_active_per_owner: usize) {
        self.max_active_per_owner
            .store(max_active_per_owner, Ordering::Relaxed);
    }

    pub(crate) fn reserve(&self, owner: &OwnerKey) -> Result<QuotaPermit, RpcError> {
        self.slots.reserve(
            owner,
            self.max_active_per_owner.load(Ordering::Relaxed),
            "tasks_per_session",
            "task",
        )
    }

    pub(crate) fn allocate(&self) -> (u64, Ref) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        (id, Ref::new("task", id))
    }

    fn unavailable(&self) -> RpcError {
        RpcError {
            code: INTERNAL_ERROR,
            message: "task registry is quarantined; restart the kernel".into(),
            data: Some(serde_json::json!({
                "subsystem": "task_registry",
                "quarantined": true,
                "restart_required": true,
            })),
        }
    }

    fn lock_entries(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<Ref, Arc<TaskEntry>>>, RpcError> {
        if self.quarantined.load(Ordering::Acquire) || self.entries.is_poisoned() {
            self.quarantined.store(true, Ordering::Release);
            return Err(self.unavailable());
        }
        match self.entries.lock() {
            Ok(entries) => Ok(entries),
            Err(poisoned) => {
                drop(poisoned);
                self.quarantined.store(true, Ordering::Release);
                Err(self.unavailable())
            }
        }
    }

    pub(crate) fn insert_checked(&self, entry: Arc<TaskEntry>) -> Result<(), RpcError> {
        self.lock_entries()?.insert(entry.task.clone(), entry);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn insert(&self, entry: Arc<TaskEntry>) {
        let _ = self.insert_checked(entry);
    }

    pub(crate) fn remove(&self, task_ref: &Ref) -> Option<Arc<TaskEntry>> {
        self.lock_entries().ok()?.remove(task_ref)
    }

    pub(crate) fn get(&self, task_ref: &Ref) -> Result<Arc<TaskEntry>, RpcError> {
        self.lock_entries()?
            .get(task_ref)
            .cloned()
            .ok_or_else(|| RpcError {
                code: UNKNOWN_TASK,
                message: "unknown task ref".into(),
                data: None,
            })
    }

    pub(crate) fn snapshot_owner(&self, owner: &OwnerKey) -> Result<Vec<Arc<TaskEntry>>, RpcError> {
        Ok(self
            .lock_entries()?
            .values()
            .filter(|task| &task.owner == owner)
            .cloned()
            .collect())
    }

    pub(crate) fn reap_finished(&self, owner: &OwnerKey) {
        let Ok(entries) = self.lock_entries().map(|entries| {
            entries
                .iter()
                .filter(|(_, task)| &task.owner == owner)
                .map(|(task_ref, task)| (task_ref.clone(), task.clone()))
                .collect::<Vec<_>>()
        }) else {
            return;
        };
        let cutoff = now_ns().saturating_sub(RETENTION_NS);
        let mut finished = entries
            .into_iter()
            .filter_map(|(task_ref, task)| {
                let finished_ns = task_record(&task)
                    .ok()
                    .and_then(|record| record.finished_ns);
                finished_ns.map(|finished_ns| (task_ref, task, finished_ns))
            })
            .collect::<Vec<_>>();
        finished.sort_unstable_by_key(|(_, _, finished_ns)| *finished_ns);
        let retained = finished
            .iter()
            .filter(|(_, _, finished_ns)| *finished_ns >= cutoff)
            .count();
        let over_cap = retained.saturating_sub(MAX_FINISHED_PER_OWNER);
        let mut retained_removed = 0usize;
        let remove = finished
            .into_iter()
            .filter(|(_, _, finished_ns)| {
                if *finished_ns < cutoff {
                    true
                } else if retained_removed < over_cap {
                    retained_removed += 1;
                    true
                } else {
                    false
                }
            })
            .collect::<Vec<_>>();
        let removed = {
            let Ok(mut entries) = self.lock_entries() else {
                return;
            };
            remove
                .into_iter()
                .filter_map(|(task_ref, observed, _)| {
                    entries
                        .get(&task_ref)
                        .is_some_and(|current| Arc::ptr_eq(current, &observed))
                        .then(|| {
                            entries
                                .remove(&task_ref)
                                .expect("task was observed under the same registry lock")
                        })
                })
                .collect::<Vec<_>>()
        };
        drop(removed);
    }

    /// Session eviction can only target an owner with no live leases, so its
    /// terminal task metadata can be discarded with the transcript it names.
    /// An inconsistent active record is conservatively retained.
    pub(crate) fn remove_terminal_owner(&self, owner: &OwnerKey) {
        let terminal = self
            .snapshot_owner(owner)
            .unwrap_or_default()
            .into_iter()
            .filter(|task| task_record(task).is_ok_and(|record| record.finished_ns.is_some()))
            .collect::<Vec<_>>();
        let removed = {
            let Ok(mut entries) = self.lock_entries() else {
                return;
            };
            terminal
                .into_iter()
                .filter_map(|observed| {
                    let task_ref = observed.task.clone();
                    entries
                        .get(&task_ref)
                        .is_some_and(|current| Arc::ptr_eq(current, &observed))
                        .then(|| entries.remove(&task_ref).expect("task was just observed"))
                })
                .collect::<Vec<_>>()
        };
        drop(removed);
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, task_ref: &Ref) -> bool {
        self.lock_entries()
            .is_ok_and(|entries| entries.contains_key(task_ref))
    }

    #[cfg(test)]
    pub(crate) fn poison_entries_for_test(&self) {
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| {
                let _entries = self
                    .entries
                    .lock()
                    .expect("test lock should not be poisoned");
                panic!("inject task registry poison");
            });
            assert!(handle.join().is_err());
        });
    }
}

#[cfg(test)]
mod poison_tests {
    use super::*;
    use crate::SessionKey;

    fn owner() -> OwnerKey {
        SessionKey::new("principal:task-poison", "task-poison").owner()
    }

    #[test]
    fn poisoned_registry_rejects_repeated_lookup_with_restart_metadata() {
        let registry = TaskRegistry::new(1);
        registry.poison_entries_for_test();
        for _ in 0..2 {
            let error = match registry.get(&Ref::new("task", 1)) {
                Ok(_) => panic!("poisoned registry must not masquerade as unknown task"),
                Err(error) => error,
            };
            assert_eq!(error.code, INTERNAL_ERROR);
            let data = error.data.unwrap();
            assert_eq!(data["subsystem"], "task_registry");
            assert_eq!(data["restart_required"], true);
        }
    }

    #[test]
    fn poisoned_quota_rejects_admission_and_permit_drop_does_not_panic() {
        let registry = TaskRegistry::new(2);
        let owner = owner();
        let permit = registry.reserve(&owner).unwrap();
        let quota = registry.slots.clone();
        let poisoner = quota.clone();
        let thread = std::thread::spawn(move || {
            let _counts = poisoner
                .counts
                .lock()
                .expect("test lock should not be poisoned");
            panic!("inject task quota poison");
        });
        assert!(thread.join().is_err());

        drop(permit);
        for _ in 0..2 {
            let error = match registry.reserve(&owner) {
                Ok(_) => panic!("poisoned quota must reject new admission"),
                Err(error) => error,
            };
            assert_eq!(error.code, INTERNAL_ERROR);
            assert_eq!(error.data.unwrap()["subsystem"], "task_quota");
        }
    }
}
