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
    ) {
        let mut indexes = self.selected(channel).lock().unwrap();
        let index = indexes.entry(owner.clone()).or_default();
        debug_assert_eq!(
            index.len() as u64,
            seq,
            "a durable index must stay dense and aligned with its channel's seqs"
        );
        index.push(entry_id);
    }

    /// Extend a startup index while the caller holds the channel registry lock.
    pub(super) fn seed(&self, channel: DurableChannel, owner: &OwnerKey, entry_ids: &[i64]) -> u64 {
        let mut indexes = self.selected(channel).lock().unwrap();
        let index = indexes.entry(owner.clone()).or_default();
        index.extend_from_slice(entry_ids);
        index.len() as u64
    }

    pub(super) fn len(&self, channel: DurableChannel, owner: &OwnerKey) -> u64 {
        self.selected(channel)
            .lock()
            .unwrap()
            .get(owner)
            .map_or(0, |entries| entries.len() as u64)
    }

    pub(super) fn range(
        &self,
        channel: DurableChannel,
        owner: &OwnerKey,
        since: Option<u64>,
        upto: u64,
    ) -> Vec<(u64, i64)> {
        let indexes = self.selected(channel).lock().unwrap();
        let Some(index) = indexes.get(owner) else {
            return Vec::new();
        };
        let start = since.map(|seq| seq.saturating_add(1)).unwrap_or(0);
        (start..upto)
            .filter_map(|seq| index.get(seq as usize).map(|&entry_id| (seq, entry_id)))
            .collect()
    }

    pub(super) fn remove_owner(&self, owner: &OwnerKey) {
        self.journal.lock().unwrap().remove(owner);
        self.transcript.lock().unwrap().remove(owner);
    }
}
