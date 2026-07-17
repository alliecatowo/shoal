use super::super::{OwnerKey, PtyEntry, Ref, unknown_pty};
use shoal_proto::RpcError;
use shoal_proto::error_code::QUOTA_EXCEEDED;
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const MAX_TERMINAL_PER_OWNER: usize = 64;

/// Owns PTY identity, admission, ownership checks, and bounded terminal
/// tombstones. The live child/emulator mutex is never acquired under the map
/// guard, and removed objects are destroyed after it is released.
pub(crate) struct PtyRegistry {
    entries: Mutex<HashMap<Ref, Arc<PtyEntry>>>,
    slots: Arc<PtyQuota>,
    max_active_per_owner: AtomicUsize,
    max_active_per_principal: AtomicUsize,
    max_active_global: AtomicUsize,
    next_id: AtomicU64,
    reaper_started: Mutex<bool>,
}

impl PtyRegistry {
    pub(crate) fn new(
        max_active_per_owner: usize,
        max_active_per_principal: usize,
        max_active_global: usize,
    ) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            slots: Arc::new(PtyQuota::default()),
            max_active_per_owner: AtomicUsize::new(max_active_per_owner),
            max_active_per_principal: AtomicUsize::new(max_active_per_principal),
            max_active_global: AtomicUsize::new(max_active_global),
            next_id: AtomicU64::new(1),
            reaper_started: Mutex::new(false),
        }
    }

    pub(crate) fn configure(
        &self,
        max_active_per_owner: usize,
        max_active_per_principal: usize,
        max_active_global: usize,
    ) {
        self.max_active_per_owner
            .store(max_active_per_owner, Ordering::Relaxed);
        self.max_active_per_principal
            .store(max_active_per_principal, Ordering::Relaxed);
        self.max_active_global
            .store(max_active_global, Ordering::Relaxed);
    }

    pub(crate) fn reserve(&self, owner: &OwnerKey) -> Result<PtyPermit, RpcError> {
        self.slots.reserve(
            owner,
            self.max_active_per_owner.load(Ordering::Relaxed),
            self.max_active_per_principal.load(Ordering::Relaxed),
            self.max_active_global.load(Ordering::Relaxed),
        )
    }

    /// Start the one registry-wide terminal sweeper. This replaces the former
    /// thread-per-PTY watcher: every kernel has at most one idle reaper thread,
    /// regardless of how many terminals it owns.
    pub(crate) fn ensure_reaper(self: &Arc<Self>) -> io::Result<()> {
        let mut started = self.reaper_started.lock().unwrap();
        if *started {
            return Ok(());
        }
        let weak = Arc::downgrade(self);
        std::thread::Builder::new()
            .name("shoal-pty-reaper".into())
            .spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(100));
                    let Some(registry) = weak.upgrade() else {
                        break;
                    };
                    registry.sweep_terminal();
                    // Do not carry the only strong reference across sleep;
                    // kernel teardown must be able to stop this thread.
                    drop(registry);
                }
            })?;
        *started = true;
        Ok(())
    }

    pub(crate) fn allocate(&self) -> (u64, Ref) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        (id, Ref::new("pty", id))
    }

    pub(crate) fn insert(&self, pty_ref: Ref, entry: Arc<PtyEntry>) {
        self.entries.lock().unwrap().insert(pty_ref, entry);
    }

    pub(crate) fn remove(&self, pty_ref: &Ref) -> Option<Arc<PtyEntry>> {
        self.entries.lock().unwrap().remove(pty_ref)
    }

    pub(crate) fn get_owned(
        &self,
        pty_ref: &Ref,
        owner: &OwnerKey,
    ) -> Result<Arc<PtyEntry>, RpcError> {
        let entry = self
            .entries
            .lock()
            .unwrap()
            .get(pty_ref)
            .cloned()
            .ok_or_else(unknown_pty)?;
        if &entry.owner != owner {
            return Err(unknown_pty());
        }
        Ok(entry)
    }

    pub(crate) fn take_owned(
        &self,
        pty_ref: &Ref,
        owner: &OwnerKey,
    ) -> Result<Arc<PtyEntry>, RpcError> {
        let mut entries = self.entries.lock().unwrap();
        if !entries
            .get(pty_ref)
            .is_some_and(|entry| &entry.owner == owner)
        {
            return Err(unknown_pty());
        }
        Ok(entries
            .remove(pty_ref)
            .expect("owned PTY observed under the same registry lock"))
    }

    pub(crate) fn snapshot_owner(&self, owner: &OwnerKey) -> Vec<(Ref, Arc<PtyEntry>)> {
        self.entries
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, entry)| &entry.owner == owner)
            .map(|(pty_ref, entry)| (pty_ref.clone(), entry.clone()))
            .collect()
    }

    pub(crate) fn reap_terminal(&self, owner: &OwnerKey) {
        let mut terminal = self
            .snapshot_owner(owner)
            .into_iter()
            .filter_map(|(pty_ref, entry)| {
                let alive = entry.session.lock().unwrap().alive();
                if !alive {
                    entry.mark_terminal();
                }
                entry
                    .terminal_since()
                    .map(|terminal_since| (pty_ref, entry, terminal_since))
            })
            .collect::<Vec<_>>();
        if terminal.len() <= MAX_TERMINAL_PER_OWNER {
            return;
        }
        terminal.sort_unstable_by_key(|(_, _, terminal_since)| *terminal_since);
        let remove = terminal.len() - MAX_TERMINAL_PER_OWNER;
        let removed = {
            let mut entries = self.entries.lock().unwrap();
            terminal
                .into_iter()
                .take(remove)
                .filter_map(|(pty_ref, observed, _)| {
                    entries
                        .get(&pty_ref)
                        .is_some_and(|current| Arc::ptr_eq(current, &observed))
                        .then(|| entries.remove(&pty_ref).expect("PTY was just observed"))
                })
                .collect::<Vec<_>>()
        };
        drop(removed);
    }

    fn sweep_terminal(&self) {
        let snapshot = self
            .entries
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut terminal_owners = HashSet::new();
        for entry in snapshot {
            if !entry.session.lock().unwrap().alive() {
                entry.mark_terminal();
                terminal_owners.insert(entry.owner.clone());
            }
        }
        for owner in terminal_owners {
            self.reap_terminal(&owner);
        }
    }

    pub(crate) fn remove_terminal_owner(&self, owner: &OwnerKey) {
        let terminal = self
            .snapshot_owner(owner)
            .into_iter()
            .filter(|(_, entry)| entry.terminal_since().is_some())
            .collect::<Vec<_>>();
        let removed = {
            let mut entries = self.entries.lock().unwrap();
            terminal
                .into_iter()
                .filter_map(|(pty_ref, observed)| {
                    entries
                        .get(&pty_ref)
                        .is_some_and(|current| Arc::ptr_eq(current, &observed))
                        .then(|| entries.remove(&pty_ref).expect("PTY was just observed"))
                })
                .collect::<Vec<_>>()
        };
        drop(removed);
    }

    #[cfg(test)]
    pub(crate) fn get(&self, pty_ref: &Ref) -> Option<Arc<PtyEntry>> {
        self.entries.lock().unwrap().get(pty_ref).cloned()
    }

    #[cfg(test)]
    pub(crate) fn active_for(&self, owner: &OwnerKey) -> bool {
        self.slots.state.lock().unwrap().owners.contains_key(owner)
    }
}

#[derive(Default)]
struct PtyQuota {
    state: Mutex<PtyQuotaState>,
}

#[derive(Default)]
struct PtyQuotaState {
    owners: HashMap<OwnerKey, usize>,
    principals: HashMap<String, usize>,
    global: usize,
}

pub(crate) struct PtyPermit {
    quota: Arc<PtyQuota>,
    owner: OwnerKey,
}

impl PtyQuota {
    fn reserve(
        self: &Arc<Self>,
        owner: &OwnerKey,
        max_owner: usize,
        max_principal: usize,
        max_global: usize,
    ) -> Result<PtyPermit, RpcError> {
        let mut state = self.state.lock().unwrap();
        let principal = &owner.0.principal;
        let owner_active = state.owners.get(owner).copied().unwrap_or(0);
        let principal_active = state.principals.get(principal).copied().unwrap_or(0);
        let quota_error = |limit: &'static str, max: usize, message: String| RpcError {
            code: QUOTA_EXCEEDED,
            message,
            data: Some(serde_json::json!({"limit": limit, "max": max})),
        };
        if state.global >= max_global {
            return Err(quota_error(
                "ptys_global",
                max_global,
                format!("kernel has reached the {max_global}-PTY global limit"),
            ));
        }
        if principal_active >= max_principal {
            return Err(quota_error(
                "ptys_per_principal",
                max_principal,
                format!("principal has reached the {max_principal}-PTY limit"),
            ));
        }
        if owner_active >= max_owner {
            return Err(quota_error(
                "ptys_per_session",
                max_owner,
                format!("session has reached the {max_owner}-PTY limit"),
            ));
        }
        state.global += 1;
        *state.principals.entry(principal.clone()).or_default() += 1;
        *state.owners.entry(owner.clone()).or_default() += 1;
        Ok(PtyPermit {
            quota: self.clone(),
            owner: owner.clone(),
        })
    }
}

impl Drop for PtyPermit {
    fn drop(&mut self) {
        let mut state = self.quota.state.lock().unwrap();
        state.global = state.global.saturating_sub(1);
        if let Some(current) = state.principals.get_mut(&self.owner.0.principal) {
            *current = current.saturating_sub(1);
            if *current == 0 {
                state.principals.remove(&self.owner.0.principal);
            }
        }
        if let Some(current) = state.owners.get_mut(&self.owner) {
            *current = current.saturating_sub(1);
            if *current == 0 {
                state.owners.remove(&self.owner);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionKey;

    fn owner(principal: &str, session: &str) -> OwnerKey {
        OwnerKey(SessionKey::new(principal, session))
    }

    #[test]
    fn pty_quota_is_atomic_across_session_principal_and_global_scopes() {
        let registry = PtyRegistry::new(2, 2, 3);
        let first = registry.reserve(&owner("agent:a", "one")).unwrap();
        let second = registry.reserve(&owner("agent:a", "two")).unwrap();
        let principal_error = registry.reserve(&owner("agent:a", "three")).err().unwrap();
        assert_eq!(principal_error.data.unwrap()["limit"], "ptys_per_principal");

        let third = registry.reserve(&owner("agent:b", "one")).unwrap();
        let global_error = registry.reserve(&owner("agent:c", "one")).err().unwrap();
        assert_eq!(global_error.data.unwrap()["limit"], "ptys_global");

        drop(first);
        let replacement = registry.reserve(&owner("agent:a", "three")).unwrap();
        drop((second, third, replacement));
        assert_eq!(registry.slots.state.lock().unwrap().global, 0);
    }

    #[test]
    fn pty_session_quota_remains_the_narrowest_owner_boundary() {
        let registry = PtyRegistry::new(1, 4, 8);
        let first = registry.reserve(&owner("agent:a", "one")).unwrap();
        let error = registry.reserve(&owner("agent:a", "one")).err().unwrap();
        assert_eq!(error.data.unwrap()["limit"], "ptys_per_session");
        drop(first);
        assert!(registry.reserve(&owner("agent:a", "one")).is_ok());
    }
}
