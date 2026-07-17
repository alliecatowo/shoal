use super::super::OwnerKey;
use super::super::StoredPlan;
use serde_json::json;
use shoal_proto::RpcError;
use shoal_proto::error_code::INTERNAL_ERROR;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Stored plan objects and their one-way authorization transitions. Callers
/// operate through bounded transactions; the mutex/map and its guard never
/// escape this service.
pub(crate) struct PlanRegistry {
    entries: Mutex<HashMap<String, StoredPlan>>,
    next_id: AtomicU64,
    quarantined: AtomicBool,
}

impl PlanRegistry {
    pub(crate) fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            quarantined: AtomicBool::new(false),
        }
    }

    pub(crate) fn allocate_ref(&self, plan_hash: &str) -> String {
        let object_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("plan:{plan_hash}:{object_id:016x}")
    }

    pub(crate) fn transaction<R>(
        &self,
        operation: impl FnOnce(&mut HashMap<String, StoredPlan>) -> R,
    ) -> Result<R, RpcError> {
        if self.quarantined.load(Ordering::Acquire) || self.entries.is_poisoned() {
            self.quarantined.store(true, Ordering::Release);
            return Err(plan_registry_poisoned());
        }
        let mut entries = self.entries.lock().map_err(|_| {
            self.quarantined.store(true, Ordering::Release);
            plan_registry_poisoned()
        })?;
        for plan in entries.values_mut() {
            plan.recover_stale_grant();
        }
        Ok(operation(&mut entries))
    }

    /// Plan refs are session-scoped state. Remove them with an evicted session
    /// rather than letting a later session generation inherit stale pending or
    /// approved objects that merely share its visible name.
    pub(crate) fn remove_owner(&self, owner: &OwnerKey) {
        if self.quarantined.load(Ordering::Acquire) || self.entries.is_poisoned() {
            self.quarantined.store(true, Ordering::Release);
            return;
        }
        let removed = {
            let Ok(mut entries) = self.entries.lock() else {
                self.quarantined.store(true, Ordering::Release);
                return;
            };
            let refs = entries
                .iter()
                .filter(|(_, plan)| {
                    plan.principal == owner.0.principal && plan.session == owner.0.name
                })
                .map(|(plan_ref, _)| plan_ref.clone())
                .collect::<Vec<_>>();
            refs.into_iter()
                .filter_map(|plan_ref| entries.remove(&plan_ref))
                .collect::<Vec<_>>()
        };
        drop(removed);
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, plan_ref: &str) -> bool {
        self.entries
            .lock()
            .is_ok_and(|entries| entries.contains_key(plan_ref))
    }

    #[cfg(test)]
    pub(crate) fn poison_for_test(&self) {
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| {
                let _entries = self.entries.lock().expect("plan registry test lock");
                panic!("inject plan registry poison");
            });
            assert!(handle.join().is_err());
        });
    }
}

fn plan_registry_poisoned() -> RpcError {
    RpcError {
        code: INTERNAL_ERROR,
        message: "plan registry is quarantined; restart the kernel".into(),
        data: Some(json!({"subsystem": "plans", "quarantined": true})),
    }
}
