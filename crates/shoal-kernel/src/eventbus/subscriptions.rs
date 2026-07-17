use super::*;
use std::collections::BTreeMap;

/// A per-connection socket writer shared between request/response dispatch and
/// the connection's single event-dispatch thread.
pub(crate) type SharedWriter = Arc<Mutex<UnixStream>>;

#[derive(Clone, PartialEq, Eq, Hash)]
struct SubscriptionKey {
    owner: OwnerKey,
    channel: String,
}

/// Bound on one channel subscription's outgoing queue.
pub(super) const SUB_QUEUE_CAP: usize = 256;

/// A replay handshake is normally only a channel-ring copy, but remain bounded
/// if a test hook, scheduler stall, or unusually hot publisher delays it.
const STAGED_EVENT_CAP: usize = EVENT_RING_CAP + SUB_QUEUE_CAP;

/// A bounded FIFO for one `(owner, channel)` subscription. Live publication
/// begins as soon as the subscription is registered, but stays in `pending`
/// until the initial replay snapshot is merged. Sequence-keyed merging makes
/// the registration/replay boundary exact: no duplicate, gap, or reordering
/// when a publish races that snapshot.
pub(super) struct SubQueue {
    channel: String,
    pub(super) state: Mutex<SubQueueState>,
}

pub(super) struct SubQueueState {
    ready: VecDeque<Event>,
    pending: BTreeMap<u64, Event>,
    replaying: bool,
    next_seq: Option<u64>,
    overflow_through: Option<u64>,
    dropped: u64,
    latest_dropped_seq: u64,
    closed: bool,
}

impl SubQueue {
    pub(super) fn new(channel: String) -> Arc<Self> {
        Arc::new(Self {
            channel,
            state: Mutex::new(SubQueueState {
                ready: VecDeque::new(),
                pending: BTreeMap::new(),
                replaying: true,
                next_seq: None,
                overflow_through: None,
                dropped: 0,
                latest_dropped_seq: 0,
                closed: false,
            }),
        })
    }

    pub(super) fn push_live(&self, event: Event) {
        let mut state = self.lock_or_close();
        if state.closed {
            return;
        }
        Self::stage(&mut state, event);
        if !state.replaying {
            Self::drain_pending(&mut state);
        }
    }

    pub(super) fn finish_replay(&self, replay: Vec<Event>) {
        let mut state = self.lock_or_close();
        if state.closed || !state.replaying {
            return;
        }
        for event in replay {
            Self::stage(&mut state, event);
        }
        state.replaying = false;
        state.next_seq = state.pending.first_key_value().map(|(&seq, _)| seq);
        Self::drain_pending(&mut state);
    }

    fn stage(state: &mut SubQueueState, event: Event) {
        if state.next_seq.is_some_and(|next| event.seq < next)
            || state.pending.contains_key(&event.seq)
        {
            return;
        }
        if state.pending.len() >= STAGED_EVENT_CAP {
            state.overflow_through = Some(
                state
                    .overflow_through
                    .map_or(event.seq, |through| through.max(event.seq)),
            );
            return;
        }
        state.pending.insert(event.seq, event);
    }

    fn drain_pending(state: &mut SubQueueState) {
        if state.next_seq.is_none() {
            state.next_seq = state.pending.first_key_value().map(|(&seq, _)| seq);
        }
        while let Some(next) = state.next_seq {
            if let Some(event) = state.pending.remove(&next) {
                Self::push_ready(state, event);
                state.next_seq = Some(next.saturating_add(1));
                continue;
            }
            if state
                .overflow_through
                .is_some_and(|through| next <= through)
            {
                let through = state.overflow_through.take().unwrap();
                let dropped = through.saturating_sub(next).saturating_add(1);
                state.dropped = state.dropped.saturating_add(dropped);
                state.latest_dropped_seq = through;
                state.pending.retain(|&seq, _| seq > through);
                state.next_seq = Some(through.saturating_add(1));
                continue;
            }
            break;
        }
    }

    fn push_ready(state: &mut SubQueueState, event: Event) {
        // Once overflow starts, keep coalescing until its summary is emitted.
        // Otherwise a newly freed slot could let a later concrete event pass
        // the summary, making the observed sequence go backwards.
        if state.ready.len() < SUB_QUEUE_CAP && state.dropped == 0 {
            state.ready.push_back(event);
        } else {
            state.dropped = state.dropped.saturating_add(1);
            state.latest_dropped_seq = event.seq;
        }
    }

    pub(super) fn pop(&self) -> Option<Event> {
        let mut state = self.lock_or_close();
        if state.replaying || state.closed {
            return None;
        }
        if let Some(event) = state.ready.pop_front() {
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
        None
    }

    fn close(&self) {
        self.lock_or_close().closed = true;
    }

    fn lock_or_close(&self) -> std::sync::MutexGuard<'_, SubQueueState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                state.ready.clear();
                state.pending.clear();
                state.closed = true;
                self.state.clear_poison();
                state
            }
        }
    }
}

struct DispatcherWake {
    epoch: u64,
    closed: bool,
}

struct DispatcherCore {
    writer: SharedWriter,
    subscriptions: Mutex<HashMap<SubscriptionKey, Arc<SubQueue>>>,
    order: Mutex<Vec<SubscriptionKey>>,
    cursor: AtomicUsize,
    wake: Mutex<DispatcherWake>,
    ready: Condvar,
    stopped: AtomicBool,
}

impl DispatcherCore {
    fn new(writer: SharedWriter) -> Arc<Self> {
        Arc::new(Self {
            writer,
            subscriptions: Mutex::new(HashMap::new()),
            order: Mutex::new(Vec::new()),
            cursor: AtomicUsize::new(0),
            wake: Mutex::new(DispatcherWake {
                epoch: 0,
                closed: false,
            }),
            ready: Condvar::new(),
            stopped: AtomicBool::new(false),
        })
    }

    fn notify(&self) {
        let mut wake = self.wake.lock().unwrap();
        wake.epoch = wake.epoch.wrapping_add(1);
        drop(wake);
        self.ready.notify_one();
    }

    fn next_event(&self) -> Option<Event> {
        loop {
            let observed_epoch = {
                let wake = self.wake.lock().unwrap();
                if wake.closed {
                    return None;
                }
                wake.epoch
            };
            let keys = self.order.lock().unwrap().clone();
            if !keys.is_empty() {
                let start = self.cursor.fetch_add(1, Ordering::Relaxed) % keys.len();
                let queues = self.subscriptions.lock().unwrap();
                for offset in 0..keys.len() {
                    let key = &keys[(start + offset) % keys.len()];
                    if let Some(event) = queues.get(key).and_then(|queue| queue.pop()) {
                        return Some(event);
                    }
                }
            }
            let mut wake = self.wake.lock().unwrap();
            while !wake.closed && wake.epoch == observed_epoch {
                wake = self.ready.wait(wake).unwrap();
            }
            if wake.closed {
                return None;
            }
        }
    }

    fn close(&self) {
        let queues = self
            .subscriptions
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for queue in queues {
            queue.close();
        }
        let mut wake = self.wake.lock().unwrap();
        wake.closed = true;
        wake.epoch = wake.epoch.wrapping_add(1);
        drop(wake);
        self.ready.notify_one();
    }
}

struct ConnectionDispatcher {
    core: Arc<DispatcherCore>,
}

impl ConnectionDispatcher {
    fn new(writer: SharedWriter) -> Result<Arc<Self>, RpcError> {
        let dispatcher = Arc::new(Self {
            core: DispatcherCore::new(writer),
        });
        let core = dispatcher.core.clone();
        std::thread::Builder::new()
            .name("shoal-event-dispatch".into())
            .spawn(move || {
                while let Some(event) = core.next_event() {
                    let notification = json!({
                        "jsonrpc": JSONRPC,
                        "method": "event",
                        "params": &event,
                    });
                    let Ok(mut writer) = core.writer.lock() else {
                        core.close();
                        break;
                    };
                    let ok = write_json_notification(&mut writer, &notification).is_ok();
                    drop(writer);
                    if !ok {
                        core.close();
                        break;
                    }
                }
                core.stopped.store(true, Ordering::Release);
            })
            .map_err(|err| RpcError {
                code: INTERNAL_ERROR,
                message: format!("failed to start event dispatcher: {err}"),
                data: None,
            })?;
        Ok(dispatcher)
    }

    fn subscribe(&self, key: SubscriptionKey) -> (Arc<SubQueue>, bool) {
        let mut subscriptions = self.core.subscriptions.lock().unwrap();
        if let Some(existing) = subscriptions.get(&key) {
            return (existing.clone(), false);
        }
        let queue = SubQueue::new(key.channel.clone());
        subscriptions.insert(key.clone(), queue.clone());
        self.core.order.lock().unwrap().push(key);
        (queue, true)
    }

    fn deliver(&self, owner: &OwnerKey, channel: &str, event: &Event) {
        let key = SubscriptionKey {
            owner: owner.clone(),
            channel: channel.to_string(),
        };
        let queue = self.core.subscriptions.lock().unwrap().get(&key).cloned();
        if let Some(queue) = queue {
            queue.push_live(event.clone());
            self.core.notify();
        }
    }

    fn unsubscribe(&self, key: &SubscriptionKey) -> bool {
        let queue = self.core.subscriptions.lock().unwrap().remove(key);
        if let Some(queue) = queue {
            queue.close();
            self.core
                .order
                .lock()
                .unwrap()
                .retain(|current| current != key);
            self.core.notify();
        }
        self.is_empty()
    }

    fn remove_owner(&self, owner: &OwnerKey) -> bool {
        let keys = self
            .core
            .subscriptions
            .lock()
            .unwrap()
            .keys()
            .filter(|key| &key.owner == owner)
            .cloned()
            .collect::<Vec<_>>();
        for key in keys {
            self.unsubscribe(&key);
        }
        self.is_empty()
    }

    fn is_empty(&self) -> bool {
        self.core.subscriptions.lock().unwrap().is_empty()
    }

    fn close(&self) {
        self.core.close();
    }
}

pub(super) struct SubscriptionHandle {
    queue: Arc<SubQueue>,
    is_new: bool,
    dispatcher: Arc<ConnectionDispatcher>,
}

impl SubscriptionHandle {
    pub(super) fn finish_replay(self, replay: Vec<Event>) {
        if self.is_new {
            self.queue.finish_replay(replay);
            self.dispatcher.core.notify();
        }
    }

    pub(super) fn is_new(&self) -> bool {
        self.is_new
    }
}

/// Owns at most one writer/dispatcher thread per connection. Each channel on
/// that connection keeps its own bounded queue, so noisy subscriptions cannot
/// consume another subscription's memory budget. No method here is called
/// while channel or durable-index state is locked.
#[derive(Default)]
pub(super) struct SubscriptionRegistry {
    connections: Mutex<HashMap<u64, Arc<ConnectionDispatcher>>>,
}

#[cfg(test)]
pub(super) struct DispatcherProbe(Arc<DispatcherCore>);

#[cfg(test)]
impl DispatcherProbe {
    pub(super) fn stopped(&self) -> bool {
        self.0.stopped.load(Ordering::Acquire)
    }
}

impl SubscriptionRegistry {
    pub(super) fn subscribe(
        &self,
        conn: u64,
        owner: &OwnerKey,
        channel: &str,
        writer: &SharedWriter,
        max_per_session: usize,
    ) -> Result<SubscriptionHandle, RpcError> {
        let mut connections = self.connections.lock().unwrap();
        let key = SubscriptionKey {
            owner: owner.clone(),
            channel: channel.to_string(),
        };
        let existing_dispatcher = connections.get(&conn).cloned();
        let exists = existing_dispatcher.as_ref().is_some_and(|dispatcher| {
            dispatcher
                .core
                .subscriptions
                .lock()
                .unwrap()
                .contains_key(&key)
        });
        let current = connections
            .values()
            .map(|dispatcher| {
                dispatcher
                    .core
                    .subscriptions
                    .lock()
                    .unwrap()
                    .keys()
                    .filter(|key| key.owner == *owner)
                    .count()
            })
            .sum::<usize>();
        if !exists && current >= max_per_session {
            return Err(RpcError {
                code: QUOTA_EXCEEDED,
                message: format!("session has reached the {max_per_session}-subscription limit"),
                data: Some(json!({
                    "limit": "subscriptions_per_session",
                    "max": max_per_session,
                })),
            });
        }
        let dispatcher = if let Some(dispatcher) = existing_dispatcher {
            dispatcher
        } else {
            let dispatcher = ConnectionDispatcher::new(writer.clone())?;
            connections.insert(conn, dispatcher.clone());
            dispatcher
        };
        let (queue, is_new) = dispatcher.subscribe(key);
        Ok(SubscriptionHandle {
            queue,
            is_new,
            dispatcher,
        })
    }

    pub(super) fn deliver(&self, owner: &OwnerKey, channel: &str, event: &Event) {
        let dispatchers = self
            .connections
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for dispatcher in dispatchers {
            dispatcher.deliver(owner, channel, event);
        }
    }

    pub(super) fn unsubscribe(&self, conn: u64, owner: &OwnerKey, channel: &str) {
        let mut connections = self.connections.lock().unwrap();
        let Some(dispatcher) = connections.get(&conn).cloned() else {
            return;
        };
        let empty = dispatcher.unsubscribe(&SubscriptionKey {
            owner: owner.clone(),
            channel: channel.to_string(),
        });
        if empty {
            connections.remove(&conn);
            dispatcher.close();
        }
    }

    pub(super) fn remove_conn(&self, conn: u64) {
        let dispatcher = self.connections.lock().unwrap().remove(&conn);
        if let Some(dispatcher) = dispatcher {
            dispatcher.close();
        }
    }

    pub(super) fn remove_owner(&self, owner: &OwnerKey) {
        let mut connections = self.connections.lock().unwrap();
        let mut empty = Vec::new();
        for (&conn, dispatcher) in connections.iter() {
            if dispatcher.remove_owner(owner) {
                empty.push((conn, dispatcher.clone()));
            }
        }
        for (conn, dispatcher) in empty {
            connections.remove(&conn);
            dispatcher.close();
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.connections
            .lock()
            .unwrap()
            .values()
            .map(|dispatcher| dispatcher.core.subscriptions.lock().unwrap().len())
            .sum()
    }

    #[cfg(test)]
    pub(super) fn dispatcher_count(&self) -> usize {
        self.connections.lock().unwrap().len()
    }

    #[cfg(test)]
    pub(super) fn dispatcher_probe(&self, conn: u64) -> Option<DispatcherProbe> {
        self.connections
            .lock()
            .unwrap()
            .get(&conn)
            .map(|dispatcher| DispatcherProbe(dispatcher.core.clone()))
    }
}

fn write_json_notification(writer: &mut UnixStream, value: &Json) -> io::Result<()> {
    let mut buffer = serde_json::to_vec(value).map_err(io::Error::other)?;
    buffer.push(b'\n');
    use std::io::Write as _;
    writer.write_all(&buffer)?;
    writer.flush()
}
