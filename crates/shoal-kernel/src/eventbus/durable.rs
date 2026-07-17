use super::*;

/// Selects the durable seq-to-journal-entry index associated with a channel.
#[derive(Clone, Copy)]
pub(super) enum DurableChannel {
    Journal,
    Transcript,
}

/// Durable replay pointers for the two journal-backed event channels.
///
/// These locks are never acquired before the channel registry lock. Operations
/// that must update a channel cursor and its index atomically are deliberately
/// exposed only to [`ChannelRegistry`](super::channels::ChannelRegistry), which
/// establishes the global `channels -> durable index` order.
#[derive(Default)]
pub(super) struct DurableIndexes {
    journal: Mutex<HashMap<OwnerKey, Vec<i64>>>,
    transcript: Mutex<HashMap<OwnerKey, Vec<i64>>>,
    /// Poison in either index invalidates the shared seq-to-entry invariant.
    /// The index is journal-reconstructible at process startup, but not while
    /// requests are live because channel cursors may already have advanced.
    quarantined: AtomicBool,
}

impl DurableIndexes {
    fn selected(&self, channel: DurableChannel) -> &Mutex<HashMap<OwnerKey, Vec<i64>>> {
        match channel {
            DurableChannel::Journal => &self.journal,
            DurableChannel::Transcript => &self.transcript,
        }
    }

    /// Append while the caller holds the matching channel registry lock.
    pub(super) fn append(
        &self,
        channel: DurableChannel,
        owner: &OwnerKey,
        seq: u64,
        entry_id: i64,
    ) -> bool {
        if self.is_quarantined() {
            return false;
        }
        let Ok(mut indexes) = self.selected(channel).lock() else {
            self.quarantined.store(true, Ordering::Release);
            return false;
        };
        let index = indexes.entry(owner.clone()).or_default();
        debug_assert_eq!(
            index.len() as u64,
            seq,
            "a durable index must stay dense and aligned with its channel's seqs"
        );
        index.push(entry_id);
        true
    }

    /// Extend a startup index while the caller holds the channel registry lock.
    pub(super) fn seed(
        &self,
        channel: DurableChannel,
        owner: &OwnerKey,
        entry_ids: &[i64],
    ) -> Option<u64> {
        if self.is_quarantined() {
            return None;
        }
        let Ok(mut indexes) = self.selected(channel).lock() else {
            self.quarantined.store(true, Ordering::Release);
            return None;
        };
        let index = indexes.entry(owner.clone()).or_default();
        index.extend_from_slice(entry_ids);
        Some(index.len() as u64)
    }

    pub(super) fn len(&self, channel: DurableChannel, owner: &OwnerKey) -> u64 {
        if self.is_quarantined() {
            return 0;
        }
        let Ok(indexes) = self.selected(channel).lock() else {
            self.quarantined.store(true, Ordering::Release);
            return 0;
        };
        indexes.get(owner).map_or(0, |entries| entries.len() as u64)
    }

    pub(super) fn range(
        &self,
        channel: DurableChannel,
        owner: &OwnerKey,
        since: Option<u64>,
        upto: u64,
    ) -> Vec<(u64, i64)> {
        if self.is_quarantined() {
            return Vec::new();
        }
        let Ok(indexes) = self.selected(channel).lock() else {
            self.quarantined.store(true, Ordering::Release);
            return Vec::new();
        };
        let Some(index) = indexes.get(owner) else {
            return Vec::new();
        };
        let start = since.map(|seq| seq.saturating_add(1)).unwrap_or(0);
        (start..upto)
            .filter_map(|seq| index.get(seq as usize).map(|&entry_id| (seq, entry_id)))
            .collect()
    }

    pub(super) fn remove_owner(&self, owner: &OwnerKey) {
        if self.is_quarantined() {
            return;
        }
        let Ok(mut journal) = self.journal.lock() else {
            self.quarantined.store(true, Ordering::Release);
            return;
        };
        let Ok(mut transcript) = self.transcript.lock() else {
            self.quarantined.store(true, Ordering::Release);
            return;
        };
        journal.remove(owner);
        transcript.remove(owner);
    }

    pub(super) fn is_quarantined(&self) -> bool {
        self.quarantined.load(Ordering::Acquire)
            || self.journal.is_poisoned()
            || self.transcript.is_poisoned()
    }

    #[cfg(test)]
    pub(super) fn poison_journal_for_test(&self) {
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| {
                let _guard = self
                    .journal
                    .lock()
                    .expect("test lock should not be poisoned");
                panic!("inject durable-index poison");
            });
            assert!(handle.join().is_err());
        });
    }
}
