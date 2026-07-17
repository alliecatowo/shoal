use super::super::{OwnerKey, QuotaPermit, Ref, SessionQuota, TaskEntry, now_ns};
use shoal_proto::RpcError;
use shoal_proto::error_code::UNKNOWN_TASK;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const MAX_FINISHED_PER_OWNER: usize = 512;
pub(crate) const RETENTION_NS: i64 = 24 * 60 * 60 * 1_000_000_000;

/// Owns task identity, admission, lookup, and terminal retention. Registry
/// operations return cloned entries/snapshots and never expose a map guard.
pub(crate) struct TaskRegistry {
    entries: Mutex<HashMap<Ref, Arc<TaskEntry>>>,
    slots: Arc<SessionQuota>,
    max_active_per_owner: AtomicUsize,
    next_id: AtomicU64,
}

impl TaskRegistry {
    pub(crate) fn new(max_active_per_owner: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            slots: Arc::new(SessionQuota::default()),
            max_active_per_owner: AtomicUsize::new(max_active_per_owner),
            next_id: AtomicU64::new(1),
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

    pub(crate) fn insert(&self, entry: Arc<TaskEntry>) {
        self.entries
            .lock()
            .unwrap()
            .insert(entry.task.clone(), entry);
    }

    pub(crate) fn remove(&self, task_ref: &Ref) -> Option<Arc<TaskEntry>> {
        self.entries.lock().unwrap().remove(task_ref)
    }

    pub(crate) fn get(&self, task_ref: &Ref) -> Result<Arc<TaskEntry>, RpcError> {
        self.entries
            .lock()
            .unwrap()
            .get(task_ref)
            .cloned()
            .ok_or_else(|| RpcError {
                code: UNKNOWN_TASK,
                message: "unknown task ref".into(),
                data: None,
            })
    }

    pub(crate) fn snapshot_owner(&self, owner: &OwnerKey) -> Vec<Arc<TaskEntry>> {
        self.entries
            .lock()
            .unwrap()
            .values()
            .filter(|task| &task.owner == owner)
            .cloned()
            .collect()
    }

    pub(crate) fn reap_finished(&self, owner: &OwnerKey) {
        let entries = self
            .entries
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, task)| &task.owner == owner)
            .map(|(task_ref, task)| (task_ref.clone(), task.clone()))
            .collect::<Vec<_>>();
        let cutoff = now_ns().saturating_sub(RETENTION_NS);
        let mut finished = entries
            .into_iter()
            .filter_map(|(task_ref, task)| {
                let finished_ns = task.inner.lock().unwrap().finished_ns;
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
            let mut entries = self.entries.lock().unwrap();
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
            .into_iter()
            .filter(|task| task.inner.lock().unwrap().finished_ns.is_some())
            .collect::<Vec<_>>();
        let removed = {
            let mut entries = self.entries.lock().unwrap();
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
        self.entries.lock().unwrap().contains_key(task_ref)
    }
}
