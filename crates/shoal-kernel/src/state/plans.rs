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
        operation(&mut self.entries.lock().unwrap())
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, plan_ref: &str) -> bool {
        self.entries.lock().unwrap().contains_key(plan_ref)
    }
}
