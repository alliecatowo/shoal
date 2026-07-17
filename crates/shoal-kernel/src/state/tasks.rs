use super::super::{OwnerKey, QuotaPermit, Ref, SessionQuota, TaskEntry, now_ns, task_record};
use shoal_proto::RpcError;
use shoal_proto::error_code::{INTERNAL_ERROR, UNKNOWN_TASK};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
    fn poison_entries_for_test(&self) {
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
