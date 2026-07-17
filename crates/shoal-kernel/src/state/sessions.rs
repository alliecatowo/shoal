use super::super::{OwnerKey, Session, SessionKey};
use serde_json::json;
use shoal_proto::RpcError;
use shoal_proto::error_code::QUOTA_EXCEEDED;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub(crate) const MAX_SESSIONS_PER_PRINCIPAL: usize = 64;
const IDLE_SESSION_TTL_NS: i64 = 24 * 60 * 60 * 1_000_000_000;

/// Principal-private evaluator sessions plus their lifecycle policy. The
/// lifecycle mutex serializes get/create/evict decisions while the entries
/// mutex remains a narrow map guard; callbacks and object destruction always
/// happen after the map guard is released.
pub(crate) struct SessionRegistry {
    entries: Mutex<HashMap<SessionKey, Arc<Session>>>,
    lifecycle: Mutex<()>,
}

impl SessionRegistry {
    pub(crate) fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            lifecycle: Mutex::new(()),
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
        let _lifecycle = self.lifecycle.lock().unwrap();
        let existing = { self.entries.lock().unwrap().get(&key).cloned() };
        if let Some(session) = existing {
            session.touch();
            return Ok(session);
        }

        let now = super::super::now_ns();
        let mut evicted = {
            let mut entries = self.entries.lock().unwrap();
            let expired = entries
                .iter()
                .filter(|(_, session)| Arc::strong_count(session) == 1)
                .filter(|(_, session)| {
                    session.last_used_ns() < now.saturating_sub(IDLE_SESSION_TTL_NS)
                })
                .map(|(key, _)| key.clone())
                .collect::<Vec<_>>();
            expired
                .into_iter()
                .filter_map(|expired_key| {
                    entries
                        .remove(&expired_key)
                        .map(|session| (expired_key, session))
                })
                .collect::<Vec<_>>()
        };
        for (evicted_key, _) in &evicted {
            cleanup_owner(&evicted_key.owner());
        }
        evicted.clear();

        let owned = self
            .entries
            .lock()
            .unwrap()
            .keys()
            .filter(|session_key| session_key.principal == key.principal)
            .count();
        if owned >= MAX_SESSIONS_PER_PRINCIPAL {
            let victim = {
                let entries = self.entries.lock().unwrap();
                entries
                    .iter()
                    .filter(|(session_key, session)| {
                        session_key.principal == key.principal && Arc::strong_count(session) == 1
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
            let victim_session = { self.entries.lock().unwrap().remove(&victim) };
            if let Some(session) = victim_session {
                cleanup_owner(&victim.owner());
                drop(session);
            }
        }

        let session = create()?;
        self.entries.lock().unwrap().insert(key, session.clone());
        Ok(session)
    }

    #[cfg(test)]
    pub(crate) fn snapshot(&self) -> HashMap<SessionKey, Arc<Session>> {
        self.entries.lock().unwrap().clone()
    }
}
