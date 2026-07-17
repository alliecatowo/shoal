use super::super::OwnerKey;
use super::super::StoredPlan;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// Stored plan objects and their one-way authorization transitions. Callers
/// operate through bounded transactions; the mutex/map and its guard never
/// escape this service.
pub(crate) struct PlanRegistry {
    entries: Mutex<HashMap<String, StoredPlan>>,
    next_id: AtomicU64,
}

impl PlanRegistry {
    pub(crate) fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
        }
    }

    pub(crate) fn allocate_ref(&self, plan_hash: &str) -> String {
        let object_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        format!("plan:{plan_hash}:{object_id:016x}")
    }

    pub(crate) fn transaction<R>(
        &self,
        operation: impl FnOnce(&mut HashMap<String, StoredPlan>) -> R,
    ) -> R {
        let mut entries = self.entries.lock().unwrap();
        for plan in entries.values_mut() {
            plan.recover_stale_grant();
        }
        operation(&mut entries)
    }

    /// Plan refs are session-scoped state. Remove them with an evicted session
    /// rather than letting a later session generation inherit stale pending or
    /// approved objects that merely share its visible name.
    pub(crate) fn remove_owner(&self, owner: &OwnerKey) {
        let removed = {
            let mut entries = self.entries.lock().unwrap();
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
        self.entries.lock().unwrap().contains_key(plan_ref)
    }
}
