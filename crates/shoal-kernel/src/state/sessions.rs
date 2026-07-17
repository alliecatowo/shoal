use super::super::{OwnerKey, Session, SessionKey};
use serde_json::json;
use shoal_proto::RpcError;
use shoal_proto::error_code::{INTERNAL_ERROR, QUOTA_EXCEEDED};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

pub(crate) const MAX_SESSIONS_PER_PRINCIPAL: usize = 64;
const IDLE_SESSION_TTL_NS: i64 = 24 * 60 * 60 * 1_000_000_000;

/// Principal-private evaluator sessions plus their lifecycle policy. The
/// lifecycle mutex serializes get/create/evict decisions while the entries
/// mutex remains a narrow map guard; callbacks and object destruction always
/// happen after the map guard is released.
pub(crate) struct SessionRegistry {
    entries: Mutex<HashMap<SessionKey, Arc<Session>>>,
    /// A logical admission lease serializes get/create/evict decisions, but
    /// its mutex guard is never held while entries, callbacks, constructors,
    /// or Session destructors run.
    lifecycle: Mutex<bool>,
    lifecycle_ready: Condvar,
    quarantined: AtomicBool,
}

struct AdmissionLease<'a> {
    registry: &'a SessionRegistry,
}

impl Drop for AdmissionLease<'_> {
    fn drop(&mut self) {
        match self.registry.lifecycle.lock() {
            Ok(mut busy) => {
                *busy = false;
                drop(busy);
                self.registry.lifecycle_ready.notify_one();
            }
            Err(poisoned) => {
                // The lifecycle invariant is no longer knowable. Do not
                // recover its state or admit another lookup in this process.
                drop(poisoned);
                self.registry.quarantined.store(true, Ordering::Release);
                self.registry.lifecycle_ready.notify_all();
            }
        }
    }
}

impl SessionRegistry {
    pub(crate) fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            lifecycle: Mutex::new(false),
            lifecycle_ready: Condvar::new(),
            quarantined: AtomicBool::new(false),
        }
    }

    fn unavailable(&self) -> RpcError {
        RpcError {
            code: INTERNAL_ERROR,
            message: "session registry is quarantined; restart the kernel".into(),
            data: Some(json!({
                "subsystem": "session_registry",
                "quarantined": true,
                "restart_required": true,
            })),
        }
    }

    fn ensure_available(&self) -> Result<(), RpcError> {
        if self.quarantined.load(Ordering::Acquire)
            || self.lifecycle.is_poisoned()
            || self.entries.is_poisoned()
        {
            self.quarantined.store(true, Ordering::Release);
            Err(self.unavailable())
        } else {
            Ok(())
        }
    }

    fn admission(&self) -> Result<AdmissionLease<'_>, RpcError> {
        self.ensure_available()?;
        let mut busy = match self.lifecycle.lock() {
            Ok(busy) => busy,
            Err(poisoned) => {
                drop(poisoned);
                self.quarantined.store(true, Ordering::Release);
                return Err(self.unavailable());
            }
        };
        while *busy {
            busy = match self.lifecycle_ready.wait(busy) {
                Ok(busy) => busy,
                Err(poisoned) => {
                    drop(poisoned);
                    self.quarantined.store(true, Ordering::Release);
                    return Err(self.unavailable());
                }
            };
            self.ensure_available()?;
        }
        *busy = true;
        drop(busy);
        Ok(AdmissionLease { registry: self })
    }

    fn lock_entries(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, HashMap<SessionKey, Arc<Session>>>, RpcError> {
        self.ensure_available()?;
        match self.entries.lock() {
            Ok(entries) => Ok(entries),
            Err(poisoned) => {
                drop(poisoned);
                self.quarantined.store(true, Ordering::Release);
                Err(self.unavailable())
            }
        }
    }

    pub(crate) fn get_or_try_insert_with<F, C>(
        &self,
        key: SessionKey,
        create: F,
        mut cleanup_owner: C,
    ) -> Result<Arc<Session>, RpcError>
    where
        F: FnOnce() -> Result<Arc<Session>, RpcError>,
        C: FnMut(&OwnerKey),
    {
        let admission = self.admission()?;
        let mut evicted = Vec::new();
        let result = (|| {
            let existing = { self.lock_entries()?.get(&key).cloned() };
            if let Some(session) = existing {
                session.touch();
                return Ok(session);
            }

            let now = super::super::now_ns();
            {
                let mut entries = self.lock_entries()?;
                let expired = entries
                    .iter()
                    .filter(|(_, session)| Arc::strong_count(session) == 1)
                    .filter(|(_, session)| {
                        session.last_used_ns() < now.saturating_sub(IDLE_SESSION_TTL_NS)
                    })
                    .map(|(key, _)| key.clone())
                    .collect::<Vec<_>>();
                evicted.extend(expired.into_iter().filter_map(|expired_key| {
                    entries
                        .remove(&expired_key)
                        .map(|session| (expired_key, session))
                }));
            }

            let owned = self
                .lock_entries()?
                .keys()
                .filter(|session_key| session_key.principal == key.principal)
                .count();
            if owned >= MAX_SESSIONS_PER_PRINCIPAL {
                let victim = {
                    let entries = self.lock_entries()?;
                    entries
                        .iter()
                        .filter(|(session_key, session)| {
                            session_key.principal == key.principal
                                && Arc::strong_count(session) == 1
                        })
                        .min_by_key(|(_, session)| session.last_used_ns())
                        .map(|(session_key, _)| session_key.clone())
                };
                let Some(victim) = victim else {
                    return Err(RpcError {
                        code: QUOTA_EXCEEDED,
                        message: format!(
                            "principal has reached the {MAX_SESSIONS_PER_PRINCIPAL}-session limit"
                        ),
                        data: Some(json!({
                            "limit": "sessions_per_principal",
                            "max": MAX_SESSIONS_PER_PRINCIPAL,
                        })),
                    });
                };
                if let Some(session) = self.lock_entries()?.remove(&victim) {
                    evicted.push((victim, session));
                }
            }

            let session = create()?;
            self.lock_entries()?.insert(key, session.clone());
            Ok(session)
        })();

        // No mutex guard is held while callbacks or Session destructors
        // acquire unrelated subsystems. Keep only the logical admission lease
        // until cleanup completes so another thread cannot recreate the same
        // owner and have its fresh state removed by an old eviction callback.
        for (evicted_key, _) in &evicted {
            cleanup_owner(&evicted_key.owner());
        }
        drop(evicted);
        drop(admission);
        result
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> HashMap<SessionKey, Arc<Session>> {
        self.lock_entries()
            .map_or_else(|_| HashMap::new(), |entries| entries.clone())
    }

    #[cfg(test)]
    fn poison_lifecycle_for_test(&self) {
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| {
                let _lifecycle = self
                    .lifecycle
                    .lock()
                    .expect("test lock should not be poisoned");
                panic!("inject session lifecycle poison");
            });
            assert!(handle.join().is_err());
        });
    }

    #[cfg(test)]
    fn poison_entries_for_test(&self) {
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| {
                let _entries = self
                    .entries
                    .lock()
                    .expect("test lock should not be poisoned");
                panic!("inject session entries poison");
            });
            assert!(handle.join().is_err());
        });
    }
}

#[cfg(test)]
mod poison_tests {
    use super::*;
    use crate::Kernel;

    fn assert_repeated_registry_failure(kernel: &Arc<Kernel>, held: &Arc<Session>) {
        assert!(
            held.ensure_healthy().is_ok(),
            "an already-held Session Arc remains independently usable"
        );
        for name in ["held", "new"] {
            let error = match kernel.session(name, "principal:registry-poison") {
                Ok(_) => panic!("all registry lookup/admission must fail closed"),
                Err(error) => error,
            };
            assert_eq!(error.code, INTERNAL_ERROR);
            let data = error.data.unwrap();
            assert_eq!(data["subsystem"], "session_registry");
            assert_eq!(data["restart_required"], true);
        }
    }

    #[test]
    fn poisoned_lifecycle_rejects_repeated_lookup_without_dropping_held_sessions() {
        let kernel = Kernel::new();
        let held = kernel.session("held", "principal:registry-poison").unwrap();
        kernel.sessions.poison_lifecycle_for_test();
        assert_repeated_registry_failure(&kernel, &held);
    }

    #[test]
    fn poisoned_entries_rejects_repeated_lookup_without_recovering_the_map() {
        let kernel = Kernel::new();
        let held = kernel.session("held", "principal:registry-poison").unwrap();
        kernel.sessions.poison_entries_for_test();
        assert_repeated_registry_failure(&kernel, &held);
    }
}
