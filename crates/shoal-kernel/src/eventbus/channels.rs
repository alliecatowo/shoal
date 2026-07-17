use super::durable::{DurableChannel, DurableIndexes};
use super::*;

/// Ring-buffered event log for one channel.
#[derive(Default)]
struct ChannelBuf {
    next_seq: u64,
    ring: VecDeque<Event>,
}

/// Owns all per-owner channel rings and their monotonic cursors.
#[derive(Default)]
pub(super) struct ChannelRegistry {
    /// Serializes multi-component publish/seed/remove operations. The buffer
    /// mutex remains narrow; this lifecycle guard makes owner cleanup atomic
    /// with respect to publication without ever involving subscriptions.
    coordination: Mutex<()>,
    buffers: Mutex<HashMap<(OwnerKey, String), ChannelBuf>>,
    /// A poisoned multi-component publication cannot be repaired in place:
    /// the cursor may have advanced while its durable pointer did not.
    quarantined: AtomicBool,
}

impl ChannelRegistry {
    /// Assign a seq and append an event, optionally updating its durable replay
    /// pointer in the same critical section.
    ///
    /// This is the sole runtime path that holds both component locks. The
    /// global order is always `channels -> durable index`; subscription state
    /// is notified only after this method returns and both locks are released.
    pub(super) fn publish(
        &self,
        durable_indexes: &DurableIndexes,
        owner: &OwnerKey,
        channel: &str,
        payload: Json,
        durable: Option<(DurableChannel, i64)>,
    ) -> Event {
        if self.is_quarantined() || durable_indexes.is_quarantined() {
            return quarantined_event(channel, payload);
        }
        let Ok(_coordination) = self.coordination.lock() else {
            self.quarantined.store(true, Ordering::Release);
            return quarantined_event(channel, payload);
        };
        let Ok(mut buffers) = self.buffers.lock() else {
            self.quarantined.store(true, Ordering::Release);
            return quarantined_event(channel, payload);
        };
        let buffer = buffers
            .entry((owner.clone(), channel.to_string()))
            .or_default();
        let seq = buffer.next_seq;
        let Some(next_seq) = seq.checked_add(1) else {
            self.quarantined.store(true, Ordering::Release);
            return quarantined_event(channel, payload);
        };
        if let Some((which, entry_id)) = durable
            && !durable_indexes.append(which, owner, seq, entry_id)
        {
            self.quarantined.store(true, Ordering::Release);
            return quarantined_event(channel, payload);
        }
        buffer.next_seq = next_seq;
        let event = Event {
            channel: channel.to_string(),
            seq,
            ts: now_ns(),
            payload,
        };
        buffer.ring.push_back(event.clone());
        while buffer.ring.len() > EVENT_RING_CAP {
            buffer.ring.pop_front();
        }
        event
    }

    pub(super) fn oldest_seq(&self, owner: &OwnerKey, channel: &str) -> Option<u64> {
        if self.is_quarantined() {
            return None;
        }
        let Ok(_coordination) = self.coordination.lock() else {
            self.quarantined.store(true, Ordering::Release);
            return None;
        };
        let Ok(buffers) = self.buffers.lock() else {
            self.quarantined.store(true, Ordering::Release);
            return None;
        };
        buffers
            .get(&(owner.clone(), channel.to_string()))
            .and_then(|buffer| buffer.ring.front().map(|event| event.seq))
    }

    pub(super) fn read(
        &self,
        owner: &OwnerKey,
        channel: &str,
        since: Option<u64>,
        limit: Option<usize>,
    ) -> Vec<Event> {
        if self.is_quarantined() {
            return Vec::new();
        }
        let Ok(_coordination) = self.coordination.lock() else {
            self.quarantined.store(true, Ordering::Release);
            return Vec::new();
        };
        let Ok(buffers) = self.buffers.lock() else {
            self.quarantined.store(true, Ordering::Release);
            return Vec::new();
        };
        let Some(buffer) = buffers.get(&(owner.clone(), channel.to_string())) else {
            return Vec::new();
        };
        let events: Vec<Event> = buffer
            .ring
            .iter()
            .filter(|event| since.is_none_or(|seq| event.seq > seq))
            .take(limit.unwrap_or(usize::MAX))
            .cloned()
            .collect();
        events
    }

    pub(super) fn next_seq(&self, owner: &OwnerKey, channel: &str) -> u64 {
        if self.is_quarantined() {
            return 0;
        }
        let Ok(_coordination) = self.coordination.lock() else {
            self.quarantined.store(true, Ordering::Release);
            return 0;
        };
        let Ok(buffers) = self.buffers.lock() else {
            self.quarantined.store(true, Ordering::Release);
            return 0;
        };
        buffers
            .get(&(owner.clone(), channel.to_string()))
            .map_or(0, |buffer| buffer.next_seq)
    }

    pub(super) fn remove_owner(&self, durable_indexes: &DurableIndexes, owner: &OwnerKey) {
        if self.is_quarantined() || durable_indexes.is_quarantined() {
            return;
        }
        let Ok(_coordination) = self.coordination.lock() else {
            self.quarantined.store(true, Ordering::Release);
            return;
        };
        let Ok(mut buffers) = self.buffers.lock() else {
            self.quarantined.store(true, Ordering::Release);
            return;
        };
        buffers.retain(|(channel_owner, _), _| channel_owner != owner);
        durable_indexes.remove_owner(owner);
    }

    /// Seed a channel cursor and its durable replay index using the same lock
    /// order as live publication, even though seeding currently runs at startup.
    pub(super) fn seed_durable(
        &self,
        durable_indexes: &DurableIndexes,
        durable_channel: DurableChannel,
        owner: &OwnerKey,
        channel: &str,
        published: u64,
        entry_ids: &[i64],
    ) -> bool {
        if self.is_quarantined() || durable_indexes.is_quarantined() {
            return false;
        }
        let Ok(_coordination) = self.coordination.lock() else {
            self.quarantined.store(true, Ordering::Release);
            return false;
        };
        let Ok(mut buffers) = self.buffers.lock() else {
            self.quarantined.store(true, Ordering::Release);
            return false;
        };
        let Some(next_seq) = durable_indexes.seed(durable_channel, owner, published, entry_ids)
        else {
            self.quarantined.store(true, Ordering::Release);
            return false;
        };
        buffers
            .entry((owner.clone(), channel.to_string()))
            .or_default()
            .next_seq = next_seq;
        true
    }

    pub(super) fn durable_len(
        &self,
        durable_indexes: &DurableIndexes,
        durable_channel: DurableChannel,
        owner: &OwnerKey,
    ) -> u64 {
        if self.is_quarantined() || durable_indexes.is_quarantined() {
            return 0;
        }
        let Ok(_coordination) = self.coordination.lock() else {
            self.quarantined.store(true, Ordering::Release);
            return 0;
        };
        durable_indexes.len(durable_channel, owner)
    }

    pub(super) fn durable_is_initialized(
        &self,
        durable_indexes: &DurableIndexes,
        durable_channel: DurableChannel,
        owner: &OwnerKey,
    ) -> bool {
        if self.is_quarantined() || durable_indexes.is_quarantined() {
            return false;
        }
        let Ok(_coordination) = self.coordination.lock() else {
            self.quarantined.store(true, Ordering::Release);
            return false;
        };
        durable_indexes.is_initialized(durable_channel, owner)
    }

    pub(super) fn durable_range(
        &self,
        durable_indexes: &DurableIndexes,
        durable_channel: DurableChannel,
        owner: &OwnerKey,
        since: Option<u64>,
        upto: u64,
    ) -> Vec<(u64, i64)> {
        if self.is_quarantined() || durable_indexes.is_quarantined() {
            return Vec::new();
        }
        let Ok(_coordination) = self.coordination.lock() else {
            self.quarantined.store(true, Ordering::Release);
            return Vec::new();
        };
        durable_indexes.range(durable_channel, owner, since, upto)
    }

    pub(super) fn is_quarantined(&self) -> bool {
        self.quarantined.load(Ordering::Acquire)
            || self.coordination.is_poisoned()
            || self.buffers.is_poisoned()
    }

    #[cfg(test)]
    pub(super) fn poison_buffers_for_test(&self) {
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| {
                let _guard = self
                    .buffers
                    .lock()
                    .expect("test lock should not be poisoned");
                panic!("inject channel-registry poison");
            });
            assert!(handle.join().is_err());
        });
    }
}

/// A fault marker used only to satisfy infallible internal publication call
/// sites. EventBus request boundaries detect quarantine and never expose or
/// deliver this marker.
fn quarantined_event(channel: &str, payload: Json) -> Event {
    Event {
        channel: channel.to_string(),
        seq: u64::MAX,
        ts: now_ns(),
        payload,
    }
}
