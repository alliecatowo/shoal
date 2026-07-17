use super::super::{OwnerKey, PtyEntry, Ref, unknown_pty};
use shoal_proto::RpcError;
use shoal_proto::error_code::{INTERNAL_ERROR, QUOTA_EXCEEDED};
use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

const MAX_TERMINAL_PER_OWNER: usize = 64;

/// Owns PTY identity, admission, ownership checks, and bounded terminal
/// tombstones. The live child/emulator mutex is never acquired under the map
/// guard, and removed objects are destroyed after it is released.
pub(crate) struct PtyRegistry {
    entries: Mutex<HashMap<Ref, Arc<PtyEntry>>>,
    quarantined: AtomicBool,
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
            quarantined: AtomicBool::new(false),
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
        self.ensure_available()?;
        self.slots.reserve(
            owner,
            self.max_active_per_owner.load(Ordering::Relaxed),
            self.max_active_per_principal.load(Ordering::Relaxed),
            self.max_active_global.load(Ordering::Relaxed),
        )
    }

    fn ensure_available(&self) -> Result<(), RpcError> {
        if self.quarantined.load(Ordering::Acquire) {
            Err(pty_unavailable("registry", "subsystem"))
        } else {
            Ok(())
        }
    }

    fn entries(&self) -> Result<MutexGuard<'_, HashMap<Ref, Arc<PtyEntry>>>, RpcError> {
        self.ensure_available()?;
        match self.entries.lock() {
            Ok(entries) => {
                if self.quarantined.load(Ordering::Acquire) {
                    drop(entries);
                    Err(pty_unavailable("registry", "subsystem"))
                } else {
                    Ok(entries)
                }
            }
            Err(poisoned) => {
                self.quarantined.store(true, Ordering::Release);
                // The identity/owner map is one registry-wide invariant. Use
                // the poisoned guard only for teardown: remove every known
                // entry so dropping them closes children and releases leases,
                // then reject every later request until kernel restart.
                let mut entries = poisoned.into_inner();
                let drained = std::mem::take(&mut *entries);
                drop(entries);
                drop(drained);
                Err(pty_unavailable("registry", "subsystem"))
            }
        }
    }

    fn quarantine_all(&self, component: &'static str) -> RpcError {
        self.quarantined.store(true, Ordering::Release);
        let drained = match self.entries.lock() {
            Ok(mut entries) => std::mem::take(&mut *entries),
            Err(poisoned) => {
                let mut entries = poisoned.into_inner();
                std::mem::take(&mut *entries)
            }
        };
        drop(drained);
        pty_unavailable(component, "subsystem")
    }

    pub(crate) fn lock_session<'a>(
        &self,
        pty_ref: &Ref,
        entry: &'a Arc<PtyEntry>,
    ) -> Result<MutexGuard<'a, shoal_exec::PtySession>, RpcError> {
        self.ensure_available()?;
        match entry.session.lock() {
            Ok(session) => Ok(session),
            Err(poisoned) => {
                drop(poisoned);
                Err(self.quarantine_entry(pty_ref, entry, "session"))
            }
        }
    }

    pub(crate) fn mark_terminal(
        &self,
        pty_ref: &Ref,
        entry: &Arc<PtyEntry>,
    ) -> Result<(), RpcError> {
        entry
            .mark_terminal()
            .map_err(|_| self.quarantine_entry(pty_ref, entry, "lifecycle"))
    }

    fn terminal_since(
        &self,
        pty_ref: &Ref,
        entry: &Arc<PtyEntry>,
    ) -> Result<Option<std::time::Instant>, RpcError> {
        entry
            .terminal_since()
            .map_err(|_| self.quarantine_entry(pty_ref, entry, "lifecycle"))
    }

    fn quarantine_entry(
        &self,
        pty_ref: &Ref,
        observed: &Arc<PtyEntry>,
        component: &'static str,
    ) -> RpcError {
        let removed = match self.entries() {
            Ok(mut entries) => {
                if entries
                    .get(pty_ref)
                    .is_some_and(|current| Arc::ptr_eq(current, observed))
                {
                    entries.remove(pty_ref)
                } else {
                    None
                }
            }
            Err(error) => return error,
        };
        if component != "lifecycle" {
            let _ = observed.mark_terminal();
        }
        drop(removed);
        pty_unavailable(component, "entry")
    }

    /// Start the one registry-wide terminal sweeper. This replaces the former
    /// thread-per-PTY watcher: every kernel has at most one idle reaper thread,
    /// regardless of how many terminals it owns.
    pub(crate) fn ensure_reaper(self: &Arc<Self>) -> Result<(), RpcError> {
        self.ensure_available()?;
        let mut started = self
            .reaper_started
            .lock()
            .map_err(|_| self.quarantine_all("reaper lifecycle"))?;
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
            })
            .map_err(internal_pty)?;
        *started = true;
        Ok(())
    }

    pub(crate) fn allocate(&self) -> (u64, Ref) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        (id, Ref::new("pty", id))
    }

    pub(crate) fn insert(&self, pty_ref: Ref, entry: Arc<PtyEntry>) -> Result<(), RpcError> {
        self.entries()?.insert(pty_ref, entry);
        Ok(())
    }

    pub(crate) fn remove(&self, pty_ref: &Ref) -> Result<Option<Arc<PtyEntry>>, RpcError> {
        Ok(self.entries()?.remove(pty_ref))
    }

    pub(crate) fn get_owned(
        &self,
        pty_ref: &Ref,
        owner: &OwnerKey,
    ) -> Result<Arc<PtyEntry>, RpcError> {
        let entry = self
            .entries()?
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
        let mut entries = self.entries()?;
        if !entries
            .get(pty_ref)
            .is_some_and(|entry| &entry.owner == owner)
        {
            return Err(unknown_pty());
        }
        entries.remove(pty_ref).ok_or_else(unknown_pty)
    }

    pub(crate) fn snapshot_owner(
        &self,
        owner: &OwnerKey,
    ) -> Result<Vec<(Ref, Arc<PtyEntry>)>, RpcError> {
        Ok(self
            .entries()?
            .iter()
            .filter(|(_, entry)| &entry.owner == owner)
            .map(|(pty_ref, entry)| (pty_ref.clone(), entry.clone()))
            .collect())
    }

    pub(crate) fn reap_terminal(&self, owner: &OwnerKey) -> Result<(), RpcError> {
        let mut terminal = self
            .snapshot_owner(owner)?
            .into_iter()
            .filter_map(|(pty_ref, entry)| {
                let alive = match self.lock_session(&pty_ref, &entry) {
                    Ok(mut session) => session.alive(),
                    Err(_) => return None,
                };
                if !alive && self.mark_terminal(&pty_ref, &entry).is_err() {
                    return None;
                }
                self.terminal_since(&pty_ref, &entry)
                    .ok()
                    .flatten()
                    .map(|terminal_since| (pty_ref, entry, terminal_since))
            })
            .collect::<Vec<_>>();
        if terminal.len() <= MAX_TERMINAL_PER_OWNER {
            return Ok(());
        }
        terminal.sort_unstable_by_key(|(_, _, terminal_since)| *terminal_since);
        let remove = terminal.len() - MAX_TERMINAL_PER_OWNER;
        let removed = {
            let mut entries = self.entries()?;
            terminal
                .into_iter()
                .take(remove)
                .filter_map(|(pty_ref, observed, _)| {
                    entries
                        .get(&pty_ref)
                        .is_some_and(|current| Arc::ptr_eq(current, &observed))
                        .then(|| entries.remove(&pty_ref))
                        .flatten()
                })
                .collect::<Vec<_>>()
        };
        drop(removed);
        Ok(())
    }

    fn sweep_terminal(&self) {
        let Ok(snapshot) = self.entries().map(|entries| {
            entries
                .iter()
                .map(|(pty_ref, entry)| (pty_ref.clone(), entry.clone()))
                .collect::<Vec<_>>()
        }) else {
            return;
        };
        let mut terminal_owners = HashSet::new();
        for (pty_ref, entry) in snapshot {
            let alive = match self.lock_session(&pty_ref, &entry) {
                Ok(mut session) => session.alive(),
                Err(_) => continue,
            };
            if !alive && self.mark_terminal(&pty_ref, &entry).is_ok() {
                terminal_owners.insert(entry.owner.clone());
            }
        }
        for owner in terminal_owners {
            let _ = self.reap_terminal(&owner);
        }
    }

    pub(crate) fn remove_terminal_owner(&self, owner: &OwnerKey) {
        let Ok(snapshot) = self.snapshot_owner(owner) else {
            return;
        };
        let terminal = snapshot
            .into_iter()
            .filter(|(pty_ref, entry)| self.terminal_since(pty_ref, entry).ok().flatten().is_some())
            .collect::<Vec<_>>();
        let removed = {
            let Ok(mut entries) = self.entries() else {
                return;
            };
            terminal
                .into_iter()
                .filter_map(|(pty_ref, observed)| {
                    entries
                        .get(&pty_ref)
                        .is_some_and(|current| Arc::ptr_eq(current, &observed))
                        .then(|| entries.remove(&pty_ref))
                        .flatten()
                })
                .collect::<Vec<_>>()
        };
        drop(removed);
    }

    #[cfg(test)]
    pub(crate) fn get(&self, pty_ref: &Ref) -> Option<Arc<PtyEntry>> {
        self.entries().ok()?.get(pty_ref).cloned()
    }

    #[cfg(test)]
    pub(crate) fn active_for(&self, owner: &OwnerKey) -> bool {
        self.slots
            .state
            .lock()
            .ok()
            .is_some_and(|state| state.owners.contains_key(owner))
    }
}

#[derive(Default)]
struct PtyQuota {
    state: Mutex<PtyQuotaState>,
    quarantined: AtomicBool,
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
        if self.quarantined.load(Ordering::Acquire) {
            return Err(pty_unavailable("quota accounting", "admission"));
        }
        let mut state = self.state.lock().map_err(|_| {
            self.quarantined.store(true, Ordering::Release);
            pty_unavailable("quota accounting", "admission")
        })?;
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
        let mut state = match self.quota.state.lock() {
            Ok(state) => state,
            Err(_) => {
                // Drop must never panic. Accounting is now unknowable, so
                // quarantine all later admission instead of guessing counts.
                self.quota.quarantined.store(true, Ordering::Release);
                return;
            }
        };
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

fn pty_unavailable(component: &'static str, scope: &'static str) -> RpcError {
    RpcError {
        code: INTERNAL_ERROR,
        message: format!("PTY {component} invariant failed; restart the kernel"),
        data: Some(serde_json::json!({
            "subsystem": "pty",
            "component": component,
            "scope": scope,
            "quarantined": true,
            "action": "restart_kernel",
        })),
    }
}

fn internal_pty(error: io::Error) -> RpcError {
    RpcError {
        code: INTERNAL_ERROR,
        message: format!("PTY reaper could not start: {error}"),
        data: Some(serde_json::json!({
            "subsystem": "pty",
            "component": "reaper",
            "quarantined": false,
        })),
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

    #[test]
    fn poisoned_registry_is_drained_and_repeated_requests_fail_typed() {
        let registry = Arc::new(PtyRegistry::new(1, 1, 1));
        let poisoner = registry.clone();
        assert!(
            std::panic::catch_unwind(move || {
                let _entries = poisoner.entries.lock().unwrap();
                panic!("inject PTY registry poison");
            })
            .is_err()
        );

        for _ in 0..2 {
            let error = registry
                .get_owned(&Ref::new("pty", 1), &owner("agent:a", "one"))
                .err()
                .unwrap();
            assert_eq!(error.code, INTERNAL_ERROR);
            let data = error.data.unwrap();
            assert_eq!(data["subsystem"], "pty");
            assert_eq!(data["component"], "registry");
            assert_eq!(data["scope"], "subsystem");
            assert_eq!(data["quarantined"], true);
        }
        assert!(registry.reserve(&owner("agent:a", "one")).is_err());
    }

    #[test]
    fn poisoned_quota_fails_closed_and_permit_drop_never_panics() {
        let registry = Arc::new(PtyRegistry::new(2, 2, 2));
        let permit = registry.reserve(&owner("agent:a", "one")).unwrap();
        let poisoner = registry.clone();
        assert!(
            std::panic::catch_unwind(move || {
                let _state = poisoner.slots.state.lock().unwrap();
                panic!("inject PTY quota poison");
            })
            .is_err()
        );
        assert!(std::panic::catch_unwind(move || drop(permit)).is_ok());

        for _ in 0..2 {
            let error = registry.reserve(&owner("agent:b", "two")).err().unwrap();
            assert_eq!(error.code, INTERNAL_ERROR);
            let data = error.data.unwrap();
            assert_eq!(data["component"], "quota accounting");
            assert_eq!(data["scope"], "admission");
            assert_eq!(data["quarantined"], true);
        }
    }

    #[test]
    fn poisoned_reaper_lifecycle_quarantines_the_registry() {
        let registry = Arc::new(PtyRegistry::new(1, 1, 1));
        let poisoner = registry.clone();
        assert!(
            std::panic::catch_unwind(move || {
                let _started = poisoner.reaper_started.lock().unwrap();
                panic!("inject PTY reaper lifecycle poison");
            })
            .is_err()
        );
        let error = registry.ensure_reaper().unwrap_err();
        assert_eq!(error.code, INTERNAL_ERROR);
        assert_eq!(error.data.unwrap()["component"], "reaper lifecycle");
        assert!(registry.reserve(&owner("agent:a", "one")).is_err());
    }
}
