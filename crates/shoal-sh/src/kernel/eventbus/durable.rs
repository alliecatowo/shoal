use super::*;

/// Selects the durable seq-to-journal-entry index associated with a channel.
#[derive(Clone, Copy)]
pub(super) enum DurableChannel {
    Journal,
    Transcript,
}

/// Maximum seq-to-entry pointers retained per owner/channel. Older pointers
/// are resolved from the journal on demand; keeping this aligned with the
/// event ring means the common hot tail never needs more pointer memory than
/// its payload ring already consumes.
pub(super) const DURABLE_POINTER_CAP: usize = super::EVENT_RING_CAP;

#[derive(Default)]
struct DurableIndex {
    /// One past the newest sequence, including pointers no longer in `tail`.
    published: u64,
    /// Sequence represented by `tail.front()` (or `published` when empty).
    base_seq: u64,
    tail: VecDeque<i64>,
}

/// Durable replay pointers for the two journal-backed event channels.
///
/// These locks are never acquired before the channel registry lock. Operations
/// that must update a channel cursor and its index atomically are deliberately
/// exposed only to [`ChannelRegistry`](super::channels::ChannelRegistry), which
/// establishes the global `channels -> durable index` order.
#[derive(Default)]
pub(super) struct DurableIndexes {
    journal: Mutex<HashMap<OwnerKey, DurableIndex>>,
    transcript: Mutex<HashMap<OwnerKey, DurableIndex>>,
    /// Poison in either index invalidates the shared seq-to-entry invariant.
    /// The index is journal-reconstructible at process startup, but not while
    /// requests are live because channel cursors may already have advanced.
    quarantined: AtomicBool,
}

impl DurableIndexes {
    fn selected(&self, channel: DurableChannel) -> &Mutex<HashMap<OwnerKey, DurableIndex>> {
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
        if index.published != seq {
            self.quarantined.store(true, Ordering::Release);
            return false;
        }
        let Some(next) = index.published.checked_add(1) else {
            self.quarantined.store(true, Ordering::Release);
            return false;
        };
        index.tail.push_back(entry_id);
        index.published = next;
        while index.tail.len() > DURABLE_POINTER_CAP {
            index.tail.pop_front();
        }
        let retained =
            u64::try_from(index.tail.len()).expect("the fixed durable pointer cap fits in u64");
        index.base_seq = index.published - retained;
        true
    }

    /// Hydrate one exact owner while the caller holds the channel registry
    /// lock. Repeated hydration is idempotent so attach/read/exec may all
    /// defensively call it without resetting a live cursor.
    pub(super) fn seed(
        &self,
        channel: DurableChannel,
        owner: &OwnerKey,
        published: u64,
        entry_ids: &[i64],
    ) -> Option<u64> {
        if self.is_quarantined() {
            return None;
        }
        let Ok(mut indexes) = self.selected(channel).lock() else {
            self.quarantined.store(true, Ordering::Release);
            return None;
        };
        if let Some(index) = indexes.get(owner) {
            if index.published == published {
                return Some(index.published);
            }
            // Hydration racing a publication that began from an unhydrated
            // zero cursor cannot be reconciled: a seq may already have been
            // observed under the wrong historical base. Fail closed.
            self.quarantined.store(true, Ordering::Release);
            return None;
        }
        let keep = entry_ids.len().min(DURABLE_POINTER_CAP);
        let Ok(keep_u64) = u64::try_from(keep) else {
            self.quarantined.store(true, Ordering::Release);
            return None;
        };
        if keep_u64 > published {
            self.quarantined.store(true, Ordering::Release);
            return None;
        }
        let tail = entry_ids[entry_ids.len() - keep..]
            .iter()
            .copied()
            .collect();
        indexes.insert(
            owner.clone(),
            DurableIndex {
                published,
                base_seq: published - keep_u64,
                tail,
            },
        );
        Some(published)
    }

    pub(super) fn len(&self, channel: DurableChannel, owner: &OwnerKey) -> u64 {
        if self.is_quarantined() {
            return 0;
        }
        let Ok(indexes) = self.selected(channel).lock() else {
            self.quarantined.store(true, Ordering::Release);
            return 0;
        };
        indexes.get(owner).map_or(0, |index| index.published)
    }

    pub(super) fn is_initialized(&self, channel: DurableChannel, owner: &OwnerKey) -> bool {
        if self.is_quarantined() {
            return false;
        }
        let Ok(indexes) = self.selected(channel).lock() else {
            self.quarantined.store(true, Ordering::Release);
            return false;
        };
        indexes.contains_key(owner)
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
        let start = since
            .map(|seq| seq.saturating_add(1))
            .unwrap_or(0)
            .max(index.base_seq);
        let end = upto.min(index.published);
        (start..end)
            .filter_map(|seq| {
                let offset = usize::try_from(seq - index.base_seq).ok()?;
                index.tail.get(offset).map(|&entry_id| (seq, entry_id))
            })
            .collect()
    }

    #[cfg(test)]
    pub(super) fn retained_len(&self, channel: DurableChannel, owner: &OwnerKey) -> usize {
        self.selected(channel)
            .lock()
            .ok()
            .and_then(|indexes| indexes.get(owner).map(|index| index.tail.len()))
            .unwrap_or(0)
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
