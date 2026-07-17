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
    buffers: Mutex<HashMap<(OwnerKey, String), ChannelBuf>>,
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
        let mut buffers = self.buffers.lock().unwrap();
        let buffer = buffers
            .entry((owner.clone(), channel.to_string()))
            .or_default();
        let seq = buffer.next_seq;
        buffer.next_seq += 1;
        if let Some((which, entry_id)) = durable {
            durable_indexes.append(which, owner, seq, entry_id);
        }
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
        self.buffers
            .lock()
            .unwrap()
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
        let buffers = self.buffers.lock().unwrap();
        let Some(buffer) = buffers.get(&(owner.clone(), channel.to_string())) else {
            return Vec::new();
        };
        let mut events: Vec<Event> = buffer
            .ring
            .iter()
            .filter(|event| since.is_none_or(|seq| event.seq > seq))
            .cloned()
            .collect();
        if let Some(limit) = limit
            && events.len() > limit
        {
            events = events.split_off(events.len() - limit);
        }
        events
    }

    pub(super) fn remove_owner(&self, owner: &OwnerKey) {
        self.buffers
            .lock()
            .unwrap()
            .retain(|(channel_owner, _), _| channel_owner != owner);
    }

    /// Seed a channel cursor and its durable replay index using the same lock
    /// order as live publication, even though seeding currently runs at startup.
    pub(super) fn seed_durable(
        &self,
        durable_indexes: &DurableIndexes,
        durable_channel: DurableChannel,
        owner: &OwnerKey,
        channel: &str,
        entry_ids: &[i64],
    ) {
        if entry_ids.is_empty() {
            return;
        }
        let mut buffers = self.buffers.lock().unwrap();
        let next_seq = durable_indexes.seed(durable_channel, owner, entry_ids);
        buffers
            .entry((owner.clone(), channel.to_string()))
            .or_default()
            .next_seq = next_seq;
    }
}
