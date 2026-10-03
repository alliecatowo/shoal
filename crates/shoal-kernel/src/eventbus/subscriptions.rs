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
pub(super) const SUB_QUEUE_MAX_BYTES: usize = 512 * 1024;
const SUBSCRIPTIONS_HARD_CAP_PER_OWNER: usize = 1024;

/// A replay handshake is normally only a channel-ring copy, but remain bounded
/// if a test hook, scheduler stall, or unusually hot publisher delays it.
const STAGED_EVENT_CAP: usize = EVENT_RING_CAP + SUB_QUEUE_CAP;
const STAGED_EVENT_MAX_BYTES: usize = SUB_QUEUE_MAX_BYTES;

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
    ready_bytes: usize,
    pending: BTreeMap<u64, Event>,
    pending_bytes: usize,
    replaying: bool,
    next_seq: Option<u64>,
    overflow_from: Option<u64>,
    overflow_through: Option<u64>,
    overflow_dropped: u64,
    overflow_dropped_bytes: u64,
    dropped: u64,
    dropped_bytes: u64,
    latest_dropped_seq: u64,
    closed: bool,
}

impl SubQueue {
    pub(super) fn new(channel: String) -> Arc<Self> {
        Arc::new(Self {
            channel,
            state: Mutex::new(SubQueueState {
                ready: VecDeque::new(),
                ready_bytes: 0,
                pending: BTreeMap::new(),
                pending_bytes: 0,
                replaying: true,
                next_seq: None,
                overflow_from: None,
                overflow_through: None,
                overflow_dropped: 0,
                overflow_dropped_bytes: 0,
                dropped: 0,
                dropped_bytes: 0,
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
        state.next_seq = match (
            state.pending.first_key_value().map(|(&seq, _)| seq),
            state.overflow_from,
        ) {
            (Some(pending), Some(overflow)) => Some(pending.min(overflow)),
            (pending, overflow) => pending.or(overflow),
        };
        Self::drain_pending(&mut state);
    }

    fn stage(state: &mut SubQueueState, event: Event) {
        if state.next_seq.is_some_and(|next| event.seq < next)
            || state.pending.contains_key(&event.seq)
        {
            return;
        }
        let event_bytes = event_retained_bytes(&event);
        let pending_bytes = state.pending_bytes.checked_add(event_bytes);
        if state.pending.len() >= STAGED_EVENT_CAP
            || pending_bytes.is_none_or(|bytes| bytes > STAGED_EVENT_MAX_BYTES)
        {
            state.overflow_from = Some(
                state
                    .overflow_from
                    .map_or(event.seq, |from| from.min(event.seq)),
            );
            state.overflow_through = Some(
                state
                    .overflow_through
                    .map_or(event.seq, |through| through.max(event.seq)),
            );
            state.overflow_dropped = state.overflow_dropped.saturating_add(1);
            state.overflow_dropped_bytes = state
                .overflow_dropped_bytes
                .saturating_add(u64::try_from(event_bytes).unwrap_or(u64::MAX));
            return;
        }
        state.pending_bytes = pending_bytes.unwrap();
        state.pending.insert(event.seq, event);
    }

    fn drain_pending(state: &mut SubQueueState) {
        if state.next_seq.is_none() {
            state.next_seq = match (
                state.pending.first_key_value().map(|(&seq, _)| seq),
                state.overflow_from,
            ) {
                (Some(pending), Some(overflow)) => Some(pending.min(overflow)),
                (pending, overflow) => pending.or(overflow),
            };
        }
        while let Some(next) = state.next_seq {
            if let Some(event) = state.pending.remove(&next) {
                let event_bytes = event_retained_bytes(&event);
                state.pending_bytes = state.pending_bytes.saturating_sub(event_bytes);
                Self::push_ready(state, event);
                state.next_seq = Some(next.saturating_add(1));
                continue;
            }
            if state
                .overflow_through
                .is_some_and(|through| next <= through)
            {
                let through = state.overflow_through.take().unwrap();
                state.overflow_from = None;
                state.dropped = state.dropped.saturating_add(state.overflow_dropped);
                state.dropped_bytes = state
                    .dropped_bytes
                    .saturating_add(state.overflow_dropped_bytes);
                state.overflow_dropped = 0;
                state.overflow_dropped_bytes = 0;
                state.latest_dropped_seq = through;
                let mut removed = 0u64;
                let mut removed_bytes = 0u64;
                state.pending.retain(|&seq, event| {
                    if seq > through {
                        true
                    } else {
                        removed = removed.saturating_add(1);
                        removed_bytes = removed_bytes.saturating_add(
                            u64::try_from(event_retained_bytes(event)).unwrap_or(u64::MAX),
                        );
                        false
                    }
                });
                state.dropped = state.dropped.saturating_add(removed);
                state.dropped_bytes = state.dropped_bytes.saturating_add(removed_bytes);
                state.pending_bytes = state
                    .pending
                    .values()
                    .map(event_retained_bytes)
                    .sum::<usize>();
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
        let event_bytes = event_retained_bytes(&event);
        let ready_bytes = state.ready_bytes.checked_add(event_bytes);
        if state.ready.len() < SUB_QUEUE_CAP
            && ready_bytes.is_some_and(|bytes| bytes <= SUB_QUEUE_MAX_BYTES)
            && state.dropped == 0
        {
            state.ready_bytes = ready_bytes.unwrap();
            state.ready.push_back(event);
        } else {
            state.dropped = state.dropped.saturating_add(1);
            state.dropped_bytes = state
                .dropped_bytes
                .saturating_add(u64::try_from(event_bytes).unwrap_or(u64::MAX));
            state.latest_dropped_seq = event.seq;
        }
    }

    pub(super) fn pop(&self) -> Option<Event> {
        let mut state = self.lock_or_close();
        if state.replaying || state.closed {
            return None;
        }
        if let Some(event) = state.ready.pop_front() {
            state.ready_bytes = state
                .ready_bytes
                .saturating_sub(event_retained_bytes(&event));
            return Some(event);
        }
        if state.dropped > 0 {
            let dropped = state.dropped;
            let latest_seq = state.latest_dropped_seq;
            let dropped_bytes = state.dropped_bytes;
            state.dropped = 0;
            state.dropped_bytes = 0;
            return Some(Event {
                channel: self.channel.clone(),
                seq: latest_seq,
                ts: now_ns(),
                payload: json!({
                    "dropped": dropped,
                    "dropped_bytes": dropped_bytes,
                    "latest_seq": latest_seq,
                }),
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
                state.ready_bytes = 0;
                state.pending.clear();
                state.pending_bytes = 0;
                state.closed = true;
                self.state.clear_poison();
                state
            }
        }
    }

    #[cfg(test)]
    pub(super) fn retained_bytes(&self) -> usize {
        self.state
            .lock()
            .map_or(0, |state| state.ready_bytes + state.pending_bytes)
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
    connection_failed: AtomicBool,
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
            connection_failed: AtomicBool::new(false),
        })
    }

    fn notify(&self) {
        if self.connection_failed.load(Ordering::Acquire) {
            return;
        }
        let mut wake = match self.wake.lock() {
            Ok(wake) => wake,
            Err(poisoned) => {
                drop(poisoned);
                self.fail_connection();
                return;
            }
        };
        wake.epoch = wake.epoch.wrapping_add(1);
        drop(wake);
        self.ready.notify_one();
    }

    fn next_event(&self) -> Option<Event> {
        loop {
            let observed_epoch = {
                let wake = match self.wake.lock() {
                    Ok(wake) => wake,
                    Err(poisoned) => {
                        drop(poisoned);
                        self.fail_connection();
                        return None;
                    }
                };
                if wake.closed {
                    return None;
                }
                wake.epoch
            };
            let keys = match self.order.lock() {
                Ok(order) => order.clone(),
                Err(poisoned) => {
                    drop(poisoned);
                    self.fail_connection();
                    return None;
                }
            };
            if !keys.is_empty() {
                let start = self.cursor.fetch_add(1, Ordering::Relaxed) % keys.len();
                let queues = match self.subscriptions.lock() {
                    Ok(queues) => queues,
                    Err(poisoned) => {
                        drop(poisoned);
                        self.fail_connection();
                        return None;
                    }
                };
                for offset in 0..keys.len() {
                    let key = &keys[(start + offset) % keys.len()];
                    if let Some(event) = queues.get(key).and_then(|queue| queue.pop()) {
                        return Some(event);
                    }
                }
            }
            let mut wake = match self.wake.lock() {
                Ok(wake) => wake,
                Err(poisoned) => {
                    drop(poisoned);
                    self.fail_connection();
                    return None;
                }
            };
            while !wake.closed && wake.epoch == observed_epoch {
                wake = match self.ready.wait(wake) {
                    Ok(wake) => wake,
                    Err(poisoned) => {
                        let mut wake = poisoned.into_inner();
                        wake.closed = true;
                        drop(wake);
                        self.wake.clear_poison();
                        self.fail_connection();
                        return None;
                    }
                };
            }
            if wake.closed {
                return None;
            }
        }
    }

    fn close(&self) {
        self.connection_failed.store(true, Ordering::Release);
        let queues: Vec<Arc<SubQueue>> = match self.subscriptions.lock() {
            Ok(mut subscriptions) => subscriptions.drain().map(|(_, queue)| queue).collect(),
            Err(poisoned) => {
                // Connection-fatal: discard every queue associated with this
                // writer, restore an empty map, and never reuse the core.
                let mut subscriptions = poisoned.into_inner();
                let queues = subscriptions.drain().map(|(_, queue)| queue).collect();
                drop(subscriptions);
                self.subscriptions.clear_poison();
                queues
            }
        };
        for queue in queues {
            queue.close();
        }
        match self.order.lock() {
            Ok(mut order) => order.clear(),
            Err(poisoned) => {
                let mut order = poisoned.into_inner();
                order.clear();
                drop(order);
                self.order.clear_poison();
            }
        }
        match self.wake.lock() {
            Ok(mut wake) => {
                wake.closed = true;
                wake.epoch = wake.epoch.wrapping_add(1);
            }
            Err(poisoned) => {
                let mut wake = poisoned.into_inner();
                wake.closed = true;
                wake.epoch = wake.epoch.wrapping_add(1);
                drop(wake);
                self.wake.clear_poison();
            }
        }
        self.ready.notify_all();
    }

    /// Poison in dispatcher bookkeeping makes the entire socket connection
    /// untrustworthy: stop notification delivery and close the shared writer
    /// so request dispatch cannot continue on a half-quarantined connection.
    fn fail_connection(&self) {
        self.close();
        match self.writer.lock() {
            Ok(writer) => {
                let _ = writer.shutdown(std::net::Shutdown::Both);
            }
            Err(poisoned) => {
                let writer = poisoned.into_inner();
                let _ = writer.shutdown(std::net::Shutdown::Both);
                drop(writer);
                self.writer.clear_poison();
            }
        }
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
                    let mut writer = match core.writer.lock() {
                        Ok(writer) => writer,
                        Err(poisoned) => {
                            // The writer itself is connection-fatal. Close it
                            // through the recovered guard, then stop the core;
                            // do not attempt to re-lock the poisoned mutex.
                            let writer = poisoned.into_inner();
                            let _ = writer.shutdown(std::net::Shutdown::Both);
                            drop(writer);
                            core.writer.clear_poison();
                            core.close();
                            break;
                        }
                    };
                    let ok = write_json_notification(&mut writer, &notification).is_ok();
                    drop(writer);
                    if !ok {
                        core.fail_connection();
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

    fn subscribe(&self, key: SubscriptionKey) -> Result<(Arc<SubQueue>, bool), RpcError> {
        if self.core.connection_failed.load(Ordering::Acquire) {
            return Err(connection_failed());
        }
        let mut subscriptions = match self.core.subscriptions.lock() {
            Ok(subscriptions) => subscriptions,
            Err(poisoned) => {
                drop(poisoned);
                self.core.fail_connection();
                return Err(connection_failed());
            }
        };
        if let Some(existing) = subscriptions.get(&key) {
            return Ok((existing.clone(), false));
        }
        let queue = SubQueue::new(key.channel.clone());
        subscriptions.insert(key.clone(), queue.clone());
        let order_result = self.core.order.lock();
        match order_result {
            Ok(mut order) => order.push(key),
            Err(poisoned) => {
                drop(poisoned);
                drop(subscriptions);
                self.core.fail_connection();
                return Err(connection_failed());
            }
        }
        Ok((queue, true))
    }

    fn deliver(&self, owner: &OwnerKey, channel: &str, event: &Event) {
        let key = SubscriptionKey {
            owner: owner.clone(),
            channel: channel.to_string(),
        };
        if self.core.connection_failed.load(Ordering::Acquire) {
            return;
        }
        let queue = match self.core.subscriptions.lock() {
            Ok(subscriptions) => subscriptions.get(&key).cloned(),
            Err(poisoned) => {
                drop(poisoned);
                self.core.fail_connection();
                return;
            }
        };
        if let Some(queue) = queue {
            queue.push_live(event.clone());
            self.core.notify();
        }
    }

    fn unsubscribe(&self, key: &SubscriptionKey) -> bool {
        let queue = match self.core.subscriptions.lock() {
            Ok(mut subscriptions) => subscriptions.remove(key),
            Err(poisoned) => {
                drop(poisoned);
                self.core.fail_connection();
                return true;
            }
        };
        if let Some(queue) = queue {
            queue.close();
            let mut order = match self.core.order.lock() {
                Ok(order) => order,
                Err(poisoned) => {
                    drop(poisoned);
                    self.core.fail_connection();
                    return true;
                }
            };
            order.retain(|current| current != key);
            self.core.notify();
        }
        self.is_empty()
    }

    fn remove_owner(&self, owner: &OwnerKey) -> bool {
        let keys = match self.core.subscriptions.lock() {
            Ok(subscriptions) => subscriptions
                .keys()
                .filter(|key| &key.owner == owner)
                .cloned()
                .collect::<Vec<_>>(),
            Err(poisoned) => {
                drop(poisoned);
                self.core.fail_connection();
                return true;
            }
        };
        for key in keys {
            self.unsubscribe(&key);
        }
        self.is_empty()
    }

    fn is_empty(&self) -> bool {
        match self.core.subscriptions.lock() {
            Ok(subscriptions) => subscriptions.is_empty(),
            Err(poisoned) => {
                drop(poisoned);
                self.core.fail_connection();
                true
            }
        }
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
    quarantined: AtomicBool,
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
        if self.quarantined.load(Ordering::Acquire) || self.connections.is_poisoned() {
            self.quarantine();
            return Err(subscription_registry_failed());
        }
        let mut connections = match self.connections.lock() {
            Ok(connections) => connections,
            Err(poisoned) => {
                drop(poisoned);
                self.quarantine();
                return Err(subscription_registry_failed());
            }
        };
        let key = SubscriptionKey {
            owner: owner.clone(),
            channel: channel.to_string(),
        };
        // A poisoned dispatcher is connection-fatal, not registry-fatal.
        // Remove only those connections before calculating the owner quota.
        let failed = connections
            .iter()
            .filter_map(|(&conn, dispatcher)| {
                dispatcher.core.subscriptions.is_poisoned().then_some(conn)
            })
            .collect::<Vec<_>>();
        for failed_conn in failed {
            if let Some(dispatcher) = connections.remove(&failed_conn) {
                dispatcher.close();
            }
        }
        let existing_dispatcher = connections.get(&conn).cloned();
        let exists = existing_dispatcher.as_ref().is_some_and(|dispatcher| {
            dispatcher
                .core
                .subscriptions
                .lock()
                .is_ok_and(|subscriptions| subscriptions.contains_key(&key))
        });
        let current = connections
            .values()
            .filter_map(|dispatcher| dispatcher.core.subscriptions.lock().ok())
            .map(|subscriptions| {
                subscriptions
                    .keys()
                    .filter(|key| key.owner == *owner)
                    .count()
            })
            .sum::<usize>();
        let effective_max = max_per_session.min(SUBSCRIPTIONS_HARD_CAP_PER_OWNER);
        if !exists && current >= effective_max {
            return Err(RpcError {
                code: QUOTA_EXCEEDED,
                message: format!("session has reached the {effective_max}-subscription limit"),
                data: Some(json!({
                    "limit": "subscriptions_per_session",
                    "max": effective_max,
                    "configured_max": max_per_session,
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
        let (queue, is_new) = dispatcher.subscribe(key)?;
        Ok(SubscriptionHandle {
            queue,
            is_new,
            dispatcher,
        })
    }

    pub(super) fn deliver(&self, owner: &OwnerKey, channel: &str, event: &Event) {
        if self.quarantined.load(Ordering::Acquire) {
            return;
        }
        let dispatchers: Vec<Arc<ConnectionDispatcher>> = match self.connections.lock() {
            Ok(connections) => connections.values().cloned().collect::<Vec<_>>(),
            Err(poisoned) => {
                drop(poisoned);
                self.quarantine();
                return;
            }
        };
        for dispatcher in dispatchers {
            dispatcher.deliver(owner, channel, event);
        }
    }

    pub(super) fn unsubscribe(&self, conn: u64, owner: &OwnerKey, channel: &str) {
        if self.quarantined.load(Ordering::Acquire) {
            return;
        }
        let mut connections = match self.connections.lock() {
            Ok(connections) => connections,
            Err(poisoned) => {
                drop(poisoned);
                self.quarantine();
                return;
            }
        };
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
        if self.quarantined.load(Ordering::Acquire) {
            return;
        }
        let dispatcher = match self.connections.lock() {
            Ok(mut connections) => connections.remove(&conn),
            Err(poisoned) => {
                drop(poisoned);
                self.quarantine();
                return;
            }
        };
        if let Some(dispatcher) = dispatcher {
            dispatcher.close();
        }
    }

    pub(super) fn remove_owner(&self, owner: &OwnerKey) {
        if self.quarantined.load(Ordering::Acquire) {
            return;
        }
        let mut connections = match self.connections.lock() {
            Ok(connections) => connections,
            Err(poisoned) => {
                drop(poisoned);
                self.quarantine();
                return;
            }
        };
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
        let connections = match self.connections.lock() {
            Ok(connections) => connections,
            Err(poisoned) => {
                drop(poisoned);
                self.quarantine();
                return 0;
            }
        };
        connections
            .values()
            .map(|dispatcher| {
                dispatcher
                    .core
                    .subscriptions
                    .lock()
                    .map_or(0, |subscriptions| subscriptions.len())
            })
            .sum()
    }

    #[cfg(test)]
    pub(super) fn dispatcher_count(&self) -> usize {
        self.connections
            .lock()
            .map_or(0, |connections| connections.len())
    }

    #[cfg(test)]
    pub(super) fn dispatcher_probe(&self, conn: u64) -> Option<DispatcherProbe> {
        self.connections
            .lock()
            .ok()?
            .get(&conn)
            .map(|dispatcher| DispatcherProbe(dispatcher.core.clone()))
    }

    /// A poisoned top-level map cannot safely retain any dispatcher. Drain
    /// and close every known connection, establish an empty map, then reject
    /// future subscription work for this process lifetime.
    fn quarantine(&self) {
        self.quarantined.store(true, Ordering::Release);
        let dispatchers: Vec<Arc<ConnectionDispatcher>> = match self.connections.lock() {
            Ok(mut connections) => connections
                .drain()
                .map(|(_, dispatcher)| dispatcher)
                .collect(),
            Err(poisoned) => {
                let mut connections = poisoned.into_inner();
                let dispatchers = connections
                    .drain()
                    .map(|(_, dispatcher)| dispatcher)
                    .collect();
                drop(connections);
                self.connections.clear_poison();
                dispatchers
            }
        };
        for dispatcher in dispatchers {
            dispatcher.close();
        }
    }

    #[cfg(test)]
    pub(super) fn poison_connections_for_test(&self) {
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| {
                let _guard = self
                    .connections
                    .lock()
                    .expect("test lock should not be poisoned");
                panic!("inject subscription-registry poison");
            });
            assert!(handle.join().is_err());
        });
    }

    #[cfg(test)]
    pub(super) fn poison_dispatcher_for_test(&self, conn: u64) {
        let dispatcher = self
            .connections
            .lock()
            .expect("test lock should not be poisoned")
            .get(&conn)
            .expect("test dispatcher exists")
            .clone();
        std::thread::scope(|scope| {
            let handle = scope.spawn(|| {
                let _guard = dispatcher
                    .core
                    .subscriptions
                    .lock()
                    .expect("test lock should not be poisoned");
                panic!("inject connection-dispatcher poison");
            });
            assert!(handle.join().is_err());
        });
    }
}

fn connection_failed() -> RpcError {
    RpcError {
        code: INTERNAL_ERROR,
        message: "event dispatcher connection is closed".into(),
        data: Some(json!({"subsystem": "event_dispatcher", "connection_closed": true})),
    }
}

fn subscription_registry_failed() -> RpcError {
    RpcError {
        code: INTERNAL_ERROR,
        message: "event subscription registry is quarantined; restart the kernel".into(),
        data: Some(json!({"subsystem": "event_subscriptions", "quarantined": true})),
    }
}

fn write_json_notification(writer: &mut UnixStream, value: &Json) -> io::Result<()> {
    let mut buffer = serde_json::to_vec(value).map_err(io::Error::other)?;
    buffer.push(b'\n');
    use std::io::Write as _;
    writer.write_all(&buffer)?;
    writer.flush()
}
