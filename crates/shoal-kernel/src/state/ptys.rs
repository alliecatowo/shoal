use super::super::{OwnerKey, PtyEntry, QuotaPermit, Ref, SessionQuota, unknown_pty};
use shoal_proto::RpcError;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const MAX_TERMINAL_PER_OWNER: usize = 64;

/// Owns PTY identity, admission, ownership checks, and bounded terminal
/// tombstones. The live child/emulator mutex is never acquired under the map
/// guard, and removed objects are destroyed after it is released.
pub(crate) struct PtyRegistry {
    entries: Mutex<HashMap<Ref, Arc<PtyEntry>>>,
    slots: Arc<SessionQuota>,
    max_active_per_owner: AtomicUsize,
    next_id: AtomicU64,
}

impl PtyRegistry {
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
            "ptys_per_session",
            "PTY",
        )
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
        self.slots.counts.lock().unwrap().contains_key(owner)
    }
}
