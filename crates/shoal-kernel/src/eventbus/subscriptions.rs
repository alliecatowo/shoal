use super::*;

/// A per-connection socket writer shared between request/response dispatch and
/// subscription push threads.
pub(crate) type SharedWriter = Arc<Mutex<UnixStream>>;

struct Subscriber {
    conn: u64,
    owner: OwnerKey,
    channel: String,
    queue: Arc<SubQueue>,
}

/// Bound on one subscriber's outgoing queue.
pub(super) const SUB_QUEUE_CAP: usize = 256;

/// A bounded FIFO plus coalesced overflow accounting for one subscriber.
pub(super) struct SubQueue {
    channel: String,
    pub(super) state: Mutex<SubQueueState>,
    ready: Condvar,
}

#[derive(Default)]
pub(super) struct SubQueueState {
    events: VecDeque<Event>,
    dropped: u64,
    latest_dropped_seq: u64,
    closed: bool,
}

impl SubQueue {
    pub(super) fn new(channel: String) -> Arc<Self> {
        Arc::new(Self {
            channel,
            state: Mutex::new(SubQueueState::default()),
            ready: Condvar::new(),
        })
    }

    pub(super) fn push(&self, event: Event) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                self.discard_poisoned_state(poisoned.into_inner());
                return;
            }
        };
        if state.events.len() < SUB_QUEUE_CAP {
            state.events.push_back(event);
        } else {
            state.dropped += 1;
            state.latest_dropped_seq = event.seq;
        }
        drop(state);
        self.ready.notify_one();
    }

    fn close(&self) {
        match self.state.lock() {
            Ok(mut state) => state.closed = true,
            Err(poisoned) => self.discard_poisoned_state(poisoned.into_inner()),
        }
        self.ready.notify_one();
    }

    pub(super) fn next(&self) -> Option<Event> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                self.discard_poisoned_state(poisoned.into_inner());
                return None;
            }
        };
        loop {
            if let Some(event) = state.events.pop_front() {
                return Some(event);
            }
            if state.dropped > 0 {
                let dropped = state.dropped;
                let latest_seq = state.latest_dropped_seq;
                state.dropped = 0;
                return Some(Event {
                    channel: self.channel.clone(),
                    seq: latest_seq,
                    ts: now_ns(),
                    payload: json!({"dropped": dropped, "latest_seq": latest_seq}),
                });
            }
            if state.closed {
                return None;
            }
            state = match self.ready.wait(state) {
                Ok(state) => state,
                Err(poisoned) => {
                    let mut state = poisoned.into_inner();
                    *state = SubQueueState {
                        closed: true,
                        ..SubQueueState::default()
                    };
                    self.state.clear_poison();
                    self.ready.notify_all();
                    return None;
                }
            };
        }
    }

    fn discard_poisoned_state(&self, mut state: std::sync::MutexGuard<'_, SubQueueState>) {
        *state = SubQueueState {
            closed: true,
            ..SubQueueState::default()
        };
        self.state.clear_poison();
        drop(state);
        self.ready.notify_all();
    }
}

/// Owns subscription identity, quota accounting, and queue lifetimes.
///
/// EventBus never calls this component while holding a channel or durable-index
/// lock. The entries lock may nest only with a subscriber's own bounded queue
/// lock, preserving unsubscribe/delivery serialization from the original code.
#[derive(Default)]
pub(super) struct SubscriptionRegistry {
    entries: Mutex<Vec<Subscriber>>,
}

impl SubscriptionRegistry {
    pub(super) fn subscribe(
        &self,
        conn: u64,
        owner: &OwnerKey,
        channel: &str,
        writer: &SharedWriter,
        max_per_session: usize,
    ) -> Result<Arc<SubQueue>, RpcError> {
        let mut entries = self.entries.lock().unwrap();
        if let Some(existing) = entries.iter().find(|subscriber| {
            subscriber.conn == conn && subscriber.owner == *owner && subscriber.channel == channel
        }) {
            return Ok(existing.queue.clone());
        }
        let current = entries
            .iter()
            .filter(|subscriber| subscriber.owner == *owner)
            .count();
        if current >= max_per_session {
            return Err(RpcError {
                code: QUOTA_EXCEEDED,
                message: format!("session has reached the {max_per_session}-subscription limit"),
                data: Some(json!({
                    "limit": "subscriptions_per_session",
                    "max": max_per_session,
                })),
            });
        }
        let queue = SubQueue::new(channel.to_string());
        entries.push(Subscriber {
            conn,
            owner: owner.clone(),
            channel: channel.to_string(),
            queue: queue.clone(),
        });
        spawn_subscriber_writer(queue.clone(), writer.clone());
        Ok(queue)
    }

    pub(super) fn deliver(&self, owner: &OwnerKey, channel: &str, event: &Event) {
        let entries = self.entries.lock().unwrap();
        for subscriber in entries
            .iter()
            .filter(|subscriber| subscriber.owner == *owner && subscriber.channel == channel)
        {
            subscriber.queue.push(event.clone());
        }
    }

    pub(super) fn unsubscribe(&self, conn: u64, owner: &OwnerKey, channel: &str) {
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|subscriber| {
            let keep = !(subscriber.conn == conn
                && subscriber.owner == *owner
                && subscriber.channel == channel);
            if !keep {
                subscriber.queue.close();
            }
            keep
        });
    }

    pub(super) fn remove_conn(&self, conn: u64) {
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|subscriber| {
            let keep = subscriber.conn != conn;
            if !keep {
                subscriber.queue.close();
            }
            keep
        });
    }

    pub(super) fn remove_owner(&self, owner: &OwnerKey) {
        let mut entries = self.entries.lock().unwrap();
        entries.retain(|subscriber| {
            let keep = &subscriber.owner != owner;
            if !keep {
                subscriber.queue.close();
            }
            keep
        });
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.entries.lock().unwrap().len()
    }
}

fn spawn_subscriber_writer(queue: Arc<SubQueue>, writer: SharedWriter) {
    std::thread::spawn(move || {
        while let Some(event) = queue.next() {
            let notification = json!({
                "jsonrpc": JSONRPC,
                "method": "event",
                "params": &event,
            });
            let Ok(mut writer) = writer.lock() else {
                queue.close();
                return;
            };
            let ok = write_json_notification(&mut writer, &notification).is_ok();
            drop(writer);
            if !ok {
                queue.close();
                return;
            }
        }
    });
}

fn write_json_notification(writer: &mut UnixStream, value: &Json) -> io::Result<()> {
    let mut buffer = serde_json::to_vec(value).map_err(io::Error::other)?;
    buffer.push(b'\n');
    use std::io::Write as _;
    writer.write_all(&buffer)?;
    writer.flush()
}
