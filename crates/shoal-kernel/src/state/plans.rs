use super::super::OwnerKey;
use serde_json::json;
use shoal_leash::Plan;
use shoal_proto::RpcError;
use shoal_proto::error_code::INTERNAL_ERROR;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub(crate) const MAX_STORED_PLANS_PER_OWNER: usize = 256;
pub(crate) const MAX_PLAN_SOURCE_BYTES_PER_OWNER: usize = 64 * 1024 * 1024;
pub(crate) const PLAN_TTL: Duration = Duration::from_secs(24 * 60 * 60);
pub(crate) const GRANT_RESERVATION_TTL: Duration = Duration::from_secs(30);

pub(crate) struct StoredPlan {
    pub(crate) src: String,
    pub(crate) session: String,
    /// The plan owner / requester that derived this plan.
    pub(crate) principal: String,
    /// Full BLAKE3 binding of source, canonical AST, plan, session, and requester.
    pub(crate) plan_hash: String,
    /// Full source digest retained separately for approval/audit projections.
    pub(crate) source_hash: String,
    pub(crate) plan: Plan,
    pub(crate) authorization: PlanAuthorization,
    pub(crate) created_at: Instant,
}

pub(crate) fn plan_expired(plan: &StoredPlan) -> bool {
    // In-flight transitions own the plan until their durable side effect is
    // resolved; purging one could orphan an approval audit or execution claim.
    !matches!(
        plan.authorization,
        PlanAuthorization::Granting { .. } | PlanAuthorization::Claimed(_)
    ) && plan.created_at.elapsed() > PLAN_TTL
}

impl StoredPlan {
    fn recover_stale_grant(&mut self) {
        let restore_policy_allowed = match &self.authorization {
            PlanAuthorization::Granting {
                restore_policy_allowed,
                started_at,
                lease,
                ..
            } if lease.upgrade().is_none() && started_at.elapsed() >= GRANT_RESERVATION_TTL => {
                Some(*restore_policy_allowed)
            }
            _ => None,
        };
        if let Some(restore_policy_allowed) = restore_policy_allowed {
            self.authorization = if restore_policy_allowed {
                PlanAuthorization::PolicyAllowed
            } else {
                PlanAuthorization::Pending
            };
        }
    }
}

/// One-way plan authorization state. Explicit approval is single-use: a
/// claim excludes concurrent/replayed applies, and consumed grants never
/// return to the approved state.
#[derive(Clone)]
pub(crate) enum PlanAuthorization {
    PolicyAllowed,
    Pending,
    Denied,
    Granting {
        record: ApprovalRecord,
        restore_policy_allowed: bool,
        started_at: Instant,
        lease: std::sync::Weak<()>,
    },
    Approved(ApprovalRecord),
    Claimed(ApprovalRecord),
    Consumed(ApprovalRecord),
}

impl PlanAuthorization {
    pub(crate) fn is_approved(&self) -> bool {
        matches!(
            self,
            Self::PolicyAllowed | Self::Approved(_) | Self::Claimed(_) | Self::Consumed(_)
        )
    }

    pub(crate) fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }

    pub(crate) fn approval(&self) -> Option<&ApprovalRecord> {
        match self {
            Self::Approved(record) | Self::Claimed(record) | Self::Consumed(record) => Some(record),
            Self::PolicyAllowed | Self::Pending | Self::Denied | Self::Granting { .. } => None,
        }
    }
}

/// Durable approval identity binding requester, plan, approver, scope, and
/// the execution that consumes the grant.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ApprovalRecord {
    pub(crate) requester: String,
    pub(crate) approver: String,
    pub(crate) plan_ref: String,
    pub(crate) plan_hash: String,
    pub(crate) source_hash: String,
    pub(crate) session: String,
    pub(crate) scope: Vec<String>,
    pub(crate) approved_at_ns: i64,
    pub(crate) grant_audit_id: i64,
    pub(crate) consumed_by: Option<i64>,
}

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
