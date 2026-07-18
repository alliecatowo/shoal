//! In-language `channel(name)` — the user-populated end of the ONE stream
//! substrate (site/content/internals/streams-channels.md, site/content/internals/kernel-protocol.md). A process-in-session
//! event bus: `channel("x").emit(v)` publishes, `.events()` subscribes as a
//! `stream<event>`, `.latest()` reads the last payload, `.take(timeout:)` blocks
//! for the next. Coordination is channels, never files.
//!
//! This is the same primitive the kernel EventBus exposes on the wire
//! (`events.publish/subscribe/read`); the shapes match (`{channel, seq, ts,
//! payload}`, ring-buffered, at-least-once, per-channel monotonic `seq`) so a
//! human's in-language channel and an agent's wire subscription are two surfaces
//! over one substrate. The bus lives on the `Evaluator` (session-scoped) and is
//! shared into spawned tasks so `on(...)`/`spawn` handlers see the same channels.

use crate::{ChildKind, Evaluator};

mod eval;
use shoal_ast::Args;
use shoal_exec::CancelToken;
use shoal_value::{
    CallArgs, CallCtx, ErrorVal, OpaqueHandling, Pull, Record, RetainedError, RetainedLimits,
    StreamGap, StreamGapReason, StreamVal, TaskVal, Upstream, VResult, Value, retained_size,
};
use std::collections::{HashMap, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// Ring depth per channel (site/content/internals/kernel-protocol.md requires ≥1024). Older history for a
/// user channel is evicted once past this — durable history is `.save(path)` or a
/// journaled channel, never an unbounded ring.
const RING_CAP: usize = 1024;

/// Per-session channel identity ceiling. Existing identities are never
/// evicted to admit a different name: that would silently discard history and
/// reset its sequence space.
const CHANNEL_CAP: usize = 64;

/// UTF-8 byte ceiling for a channel identity.
const CHANNEL_NAME_BYTES: usize = 256;

/// In addition to [`RING_CAP`], one channel retains at most this many measured
/// payload bytes. Together with [`CHANNEL_CAP`] this bounds replay rings to
/// roughly 16 MiB per evaluator session.
const RING_BYTE_CAP: usize = 256 * 1024;

/// Per-subscriber live/replay capacity. Publishers never wait for a slow
/// subscriber: when this fills, the oldest queued deliveries are discarded and
/// represented by one in-band overflow record before the newest event.
const SUBSCRIBER_CAP: usize = 256;

/// Aggregate number of live subscription queues in one evaluator session.
const LIVE_SUBSCRIBER_CAP: usize = 64;

/// Per-subscriber retained delivery budget. Together with
/// [`LIVE_SUBSCRIBER_CAP`] this bounds live queues to roughly 16 MiB/session.
const SUBSCRIBER_BYTE_CAP: usize = 256 * 1024;

/// A single publishable value must fit all three structural bounds before it
/// is cloned into a ring or subscriber queue.
const PAYLOAD_BYTE_CAP: usize = 64 * 1024;
const PAYLOAD_DEPTH_CAP: usize = 64;
const PAYLOAD_NODE_CAP: usize = 4096;

const CHANNEL_RETAINED_LIMITS: RetainedLimits = RetainedLimits {
    max_bytes: PAYLOAD_BYTE_CAP,
    max_depth: PAYLOAD_DEPTH_CAP,
    max_nodes: PAYLOAD_NODE_CAP,
    opaque: OpaqueHandling::Reject,
    allow_secret: false,
};

/// Conservative allowance for the `{channel, seq, ts, payload}` record around
/// a measured payload in each subscriber queue.
const EVENT_RECORD_OVERHEAD: usize = 512;

/// `StreamGap` has no heap fields today. Keeping an explicit conservative
/// charge makes byte accounting stable if its scalar shape grows.
const GAP_RETAINED_BYTES: usize = 128;

// The payload/name/envelope policy must always leave room for a gap marker in
// a subscriber queue, including the small extra Stored charge used on replay.
const _: () = assert!(
    PAYLOAD_BYTE_CAP + CHANNEL_NAME_BYTES + EVENT_RECORD_OVERHEAD + 1024 + GAP_RETAINED_BYTES
        <= SUBSCRIBER_BYTE_CAP
);

/// Maximum time a cancellation-aware receiver sleeps without re-checking its
/// token. A condvar notification still wakes it immediately for normal events.
const CANCEL_POLL: Duration = Duration::from_millis(25);

/// Which buffered events a new subscriber replays before going live.
#[derive(Clone, Copy)]
enum Replay {
    /// No replay — future events only (used by `.take`).
    None,
    /// Every buffered event (used by `.events()` with no cursor).
    All,
    /// Only events with `seq > n` (used by `.events(since: n)`).
    Since(u64),
}

impl Replay {
    fn from_since(since: Option<u64>) -> Replay {
        match since {
            Some(n) => Replay::Since(n),
            None => Replay::All,
        }
    }
    fn wants(&self, seq: u64) -> bool {
        match self {
            Replay::None => false,
            Replay::All => true,
            Replay::Since(n) => seq > *n,
        }
    }
}

struct Stored {
    seq: u64,
    ts_ns: i128,
    payload: Value,
    retained_bytes: usize,
}

#[derive(Default)]
struct ChannelState {
    next_seq: u64,
    ring: VecDeque<Stored>,
    ring_bytes: usize,
    subs: Vec<Subscriber>,
}

enum Delivery {
    Event {
        value: Value,
        retained_bytes: usize,
    },
    Gap {
        gap: StreamGap,
        retained_bytes: usize,
    },
}

impl Delivery {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::Event { retained_bytes, .. } | Self::Gap { retained_bytes, .. } => {
                *retained_bytes
            }
        }
    }
}

#[derive(Default)]
struct QueueState {
    items: VecDeque<Delivery>,
    retained_bytes: usize,
    closed: bool,
    poisoned: bool,
}

#[derive(Default)]
struct SubscriberQueue {
    state: Mutex<QueueState>,
    ready: Condvar,
}

#[derive(Clone)]
struct Subscriber(Arc<SubscriberQueue>);

impl Subscriber {
    fn lock_state(&self) -> MutexGuard<'_, QueueState> {
        match self.0.state.lock() {
            Ok(state) => state,
            Err(poisoned) => self.0.repair_poison(poisoned.into_inner()),
        }
    }

    fn push_gap(&self, gap: StreamGap) -> bool {
        let mut state = self.lock_state();
        if state.closed {
            return false;
        }
        state.items.push_back(Delivery::Gap {
            gap,
            retained_bytes: GAP_RETAINED_BYTES,
        });
        state.retained_bytes = state.retained_bytes.saturating_add(GAP_RETAINED_BYTES);
        self.0.ready.notify_one();
        true
    }

    fn push(&self, event: Value, event_bytes: usize) -> bool {
        let mut state = self.lock_state();
        if state.closed {
            return false;
        }

        let count_overflow = state.items.len().saturating_add(1) > SUBSCRIBER_CAP;
        let byte_overflow = state.retained_bytes.saturating_add(event_bytes) > SUBSCRIBER_BYTE_CAP;
        if count_overflow || byte_overflow {
            // Reserve two tail slots: one exact gap marker and the newest
            // event, and reserve their measured bytes. If an older marker is
            // evicted, carry its count forward so loss is never forgotten.
            let mut gap = StreamGap::new(StreamGapReason::SubscriberOverflow, 0);
            while state.items.len().saturating_add(2) > SUBSCRIBER_CAP
                || state
                    .retained_bytes
                    .saturating_add(GAP_RETAINED_BYTES)
                    .saturating_add(event_bytes)
                    > SUBSCRIBER_BYTE_CAP
            {
                match state.items.pop_front() {
                    Some(delivery) => {
                        state.retained_bytes = state
                            .retained_bytes
                            .saturating_sub(delivery.retained_bytes());
                        match delivery {
                            Delivery::Event { value: event, .. } => {
                                let mut dropped =
                                    StreamGap::new(StreamGapReason::SubscriberOverflow, 1);
                                if let Some(seq) = event_seq(&event) {
                                    dropped = dropped.with_seq_range(seq, seq);
                                }
                                gap.absorb(dropped);
                            }
                            Delivery::Gap { gap: dropped, .. } => gap.absorb(dropped),
                        }
                    }
                    None => break,
                }
            }
            state.items.push_back(Delivery::Gap {
                gap,
                retained_bytes: GAP_RETAINED_BYTES,
            });
            state.retained_bytes = state.retained_bytes.saturating_add(GAP_RETAINED_BYTES);
        }
        state.items.push_back(Delivery::Event {
            value: event,
            retained_bytes: event_bytes,
        });
        state.retained_bytes = state.retained_bytes.saturating_add(event_bytes);
        self.0.ready.notify_one();
        true
    }

    fn is_open(&self) -> bool {
        !self.lock_state().closed
    }
}

enum Received {
    Event(Value),
    Gap(StreamGap),
    Timeout,
    Closed,
    Cancelled,
    Poisoned,
}

/// The single-consumer handle for an in-language channel subscription.
/// Delivery is bounded by [`SUBSCRIBER_CAP`]; gaps are returned explicitly as
/// overflow records by the stream adapter.
pub struct EventReceiver {
    queue: Arc<SubscriberQueue>,
}

impl EventReceiver {
    fn recv(&self, timeout: Option<Duration>, cancel: Option<&CancelToken>) -> Received {
        let deadline = timeout.map(|duration| Instant::now().checked_add(duration));
        if deadline == Some(None) {
            return Received::Timeout;
        }
        let deadline = deadline.flatten();
        let mut state = self.queue.lock_state();
        loop {
            // Cancellation wins over replay/live backlog. A cancelled `on`
            // task must not run hundreds of already-queued handlers before it
            // is allowed to terminate.
            if cancel.is_some_and(CancelToken::is_cancelled) {
                return Received::Cancelled;
            }
            if state.poisoned {
                return Received::Poisoned;
            }
            if let Some(item) = state.items.pop_front() {
                state.retained_bytes = state.retained_bytes.saturating_sub(item.retained_bytes());
                return match item {
                    Delivery::Event { value, .. } => Received::Event(value),
                    Delivery::Gap { gap, .. } => Received::Gap(gap),
                };
            }
            if state.closed {
                return Received::Closed;
            }

            let wait = match deadline {
                Some(end) => {
                    let Some(remaining) = end.checked_duration_since(Instant::now()) else {
                        return Received::Timeout;
                    };
                    if cancel.is_some() {
                        remaining.min(CANCEL_POLL)
                    } else {
                        remaining
                    }
                }
                None if cancel.is_some() => CANCEL_POLL,
                None => {
                    state = self.queue.wait(state);
                    continue;
                }
            };
            let timed;
            (state, timed) = self.queue.wait_timeout(state, wait);
            if state.poisoned {
                return Received::Poisoned;
            }
            if timed.timed_out() && deadline.is_some_and(|end| Instant::now() >= end) {
                return Received::Timeout;
            }
        }
    }
}

impl Drop for EventReceiver {
    fn drop(&mut self) {
        let mut state = self.queue.lock_state();
        state.closed = true;
        self.queue.ready.notify_all();
    }
}

impl SubscriberQueue {
    /// Queue contents may be halfway through overflow compaction when a panic
    /// poisons this mutex. Discard that unknowable backlog and preserve the
    /// failure as a stable terminal state for every current/future receive.
    fn repair_poison<'a>(
        &'a self,
        mut state: MutexGuard<'a, QueueState>,
    ) -> MutexGuard<'a, QueueState> {
        state.items.clear();
        state.retained_bytes = 0;
        state.closed = true;
        state.poisoned = true;
        self.state.clear_poison();
        self.ready.notify_all();
        state
    }

    fn lock_state(&self) -> MutexGuard<'_, QueueState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => self.repair_poison(poisoned.into_inner()),
        }
    }

    fn wait<'a>(&'a self, state: MutexGuard<'a, QueueState>) -> MutexGuard<'a, QueueState> {
        match self.ready.wait(state) {
            Ok(state) => state,
            Err(poisoned) => self.repair_poison(poisoned.into_inner()),
        }
    }

    fn wait_timeout<'a>(
        &'a self,
        state: MutexGuard<'a, QueueState>,
        timeout: Duration,
    ) -> (MutexGuard<'a, QueueState>, std::sync::WaitTimeoutResult) {
        match self.ready.wait_timeout(state, timeout) {
            Ok(result) => result,
            Err(poisoned) => {
                let (state, timed) = poisoned.into_inner();
                (self.repair_poison(state), timed)
            }
        }
    }

    fn quarantine(&self) {
        let mut state = self.lock_state();
        state.items.clear();
        state.retained_bytes = 0;
        state.closed = true;
        state.poisoned = true;
        self.ready.notify_all();
    }
}

struct EventUpstream {
    channel: String,
    rx: EventReceiver,
}

impl Upstream for EventUpstream {
    fn pull(
        &mut self,
        _ctx: &mut dyn shoal_value::CallCtx,
        timeout: Option<Duration>,
    ) -> VResult<Pull> {
        Ok(match self.rx.recv(timeout, None) {
            Received::Event(v) => Pull::Item(v),
            Received::Gap(gap) => Pull::Item(overflow_record(&self.channel, gap)),
            Received::Timeout => Pull::Timeout,
            Received::Closed | Received::Cancelled => Pull::End,
            Received::Poisoned => return Err(channel_poisoned("subscriber queue")),
        })
    }
}

/// A host-installed hook mirroring in-language emits onto an external bus
/// (the kernel `EventBus`, so wire subscribers see them — site/content/internals/kernel-protocol.md
/// "one substrate" promise).
pub type EventForwarder = Box<dyn Fn(&str, &Value) + Send + Sync>;
type SharedEventForwarder = Arc<dyn Fn(&str, &Value) + Send + Sync>;

/// Session-scoped, in-process event bus backing in-language channels.
#[derive(Default)]
pub struct EventBus {
    channels: Mutex<HashMap<String, ChannelState>>,
    channels_quarantined: AtomicBool,
    /// Mirrors `user.*` emits to a hosting kernel's wire bus (see [`Self::emit`]).
    forwarder: Mutex<Option<SharedEventForwarder>>,
    forwarder_quarantined: AtomicBool,
}

impl EventBus {
    pub fn shared() -> Arc<EventBus> {
        Arc::new(EventBus::default())
    }

    /// Install the external forwarder (kernel hosting only; the standalone
    /// REPL/script binary never sets one and behaves exactly as before).
    pub fn set_forwarder(&self, f: EventForwarder) {
        let replacement = Some(Arc::from(f));
        match self.forwarder.lock() {
            Ok(mut forwarder) => *forwarder = replacement,
            Err(poisoned) => {
                *poisoned.into_inner() = replacement;
                self.forwarder.clear_poison();
            }
        }
        self.forwarder_quarantined.store(false, Ordering::Release);
    }

    /// Publish `payload` on `name`; returns the assigned monotonic `seq`. Every
    /// live subscriber receives the event record; dead subscribers (their stream
    /// dropped) are pruned. `user.*` events are additionally mirrored to the
    /// host's external bus when a forwarder is installed — the SAME
    /// client-writable rule the wire's `events.publish` enforces, so language
    /// code can never spoof a kernel-owned semantic channel
    /// (`journal`/`approval`/`session.transcript`/…) to wire subscribers.
    pub fn emit(&self, name: &str, payload: Value) -> VResult<u64> {
        validate_channel_name(name)?;
        let payload_bytes = payload_retained_size(&payload)?;
        // Resolve forwarder health before committing locally: a poisoned
        // bridge must not report failure after an event was already appended.
        let forwarder = if name.starts_with("user.") {
            self.forwarder_snapshot()?
        } else {
            None
        };
        let seq = self.publish_local(name, &payload, payload_bytes)?;
        if let Some(f) = forwarder
            && catch_unwind(AssertUnwindSafe(|| f(name, &payload))).is_err()
        {
            self.forwarder_quarantined.store(true, Ordering::Release);
            return Err(channel_poisoned("event forwarder"));
        }
        Ok(seq)
    }

    /// Publish an event that ORIGINATED on the external bus (the reverse
    /// direction of [`Self::emit`]'s mirror): ring + local subscribers only,
    /// never the forwarder — that would echo the event straight back out.
    pub fn inject(&self, name: &str, payload: Value) -> u64 {
        match self.try_inject(name, payload) {
            Ok(seq) => seq,
            Err(error) => {
                eprintln!("shoal: external event injection rejected: {error}");
                u64::MAX
            }
        }
    }

    /// Fallible host-facing form of [`Self::inject`]. New hosts should use this
    /// so a quarantined language bus is surfaced at their request boundary.
    pub fn try_inject(&self, name: &str, payload: Value) -> VResult<u64> {
        validate_channel_name(name)?;
        let payload_bytes = payload_retained_size(&payload)?;
        self.publish_local(name, &payload, payload_bytes)
    }

    fn publish_local(&self, name: &str, payload: &Value, payload_bytes: usize) -> VResult<u64> {
        let mut map = self.lock_channels()?;
        prune_closed_subscribers(&mut map);
        if map
            .get(name)
            .is_some_and(|channel| channel.next_seq > i64::MAX as u64)
        {
            self.quarantine_known_channels(&map);
            return Err(channel_poisoned("channel sequence"));
        }
        admit_channel_identity(&map, name)?;
        let st = map.entry(name.to_string()).or_default();
        let seq = st.next_seq;
        st.next_seq += 1;
        let ts_ns = now_ns();
        let stored_bytes = payload_bytes.saturating_add(std::mem::size_of::<Stored>());
        st.ring.push_back(Stored {
            seq,
            ts_ns,
            payload: payload.clone(),
            retained_bytes: stored_bytes,
        });
        st.ring_bytes = st.ring_bytes.saturating_add(stored_bytes);
        while st.ring.len() > RING_CAP || st.ring_bytes > RING_BYTE_CAP {
            if let Some(evicted) = st.ring.pop_front() {
                st.ring_bytes = st.ring_bytes.saturating_sub(evicted.retained_bytes);
            }
        }
        let event = event_record(name, seq, ts_ns, payload);
        let event_bytes = event_retained_bytes(name, payload_bytes);
        st.subs.retain(|sub| sub.push(event.clone(), event_bytes));
        Ok(seq)
    }

    /// The last payload published on `name`, or `null` if none (no wait).
    pub fn latest(&self, name: &str) -> VResult<Value> {
        validate_channel_name(name)?;
        let map = self.lock_channels()?;
        Ok(map
            .get(name)
            .and_then(|st| st.ring.back())
            .map(|s| s.payload.clone())
            .unwrap_or(Value::Null))
    }

    /// Subscribe to `name`, returning a receiver of `event` records. Replay
    /// mirrors the kernel EventBus (site/content/internals/kernel-protocol.md): `since: None` replays the
    /// whole ring then goes live; `since: Some(n)` replays only `seq > n` (the
    /// in-language `?since=` cursor, site/content/internals/streams-channels.md), then live.
    pub fn events(&self, name: &str, since: Option<u64>) -> VResult<EventReceiver> {
        validate_channel_name(name)?;
        self.subscribe(name, Replay::from_since(since))
    }

    /// Subscribe as a language stream. The custom upstream preserves the
    /// bounded queue's explicit overflow records instead of hiding it behind an
    /// unbounded `mpsc` adapter.
    pub fn event_stream(&self, name: &str, since: Option<u64>) -> VResult<StreamVal> {
        let rx = self.events(name, since)?;
        Ok(StreamVal::from_upstream(
            "event",
            false,
            Box::new(EventUpstream {
                channel: name.to_string(),
                rx,
            }),
        ))
    }

    /// Register a subscriber with the given replay policy.
    fn subscribe(&self, name: &str, replay: Replay) -> VResult<EventReceiver> {
        validate_channel_name(name)?;
        let queue = Arc::new(SubscriberQueue::default());
        let sub = Subscriber(queue.clone());
        let mut map = self.lock_channels()?;
        prune_closed_subscribers(&mut map);
        let live_subscribers = map
            .values()
            .map(|channel| channel.subs.len())
            .sum::<usize>();
        if live_subscribers >= LIVE_SUBSCRIBER_CAP {
            return Err(ErrorVal::new(
                "channel_subscriber_limit",
                format!(
                    "channel subscriber limit ({LIVE_SUBSCRIBER_CAP}) reached; drop a channel stream before subscribing again"
                ),
            ));
        }
        admit_channel_identity(&map, name)?;
        let st = map.entry(name.to_string()).or_default();
        if let Replay::Since(since) = replay {
            let expected = since.saturating_add(1);
            let first_retained = st.ring.front().map_or(st.next_seq, |event| event.seq);
            if first_retained > expected {
                let gap =
                    StreamGap::new(StreamGapReason::HistoryEvicted, first_retained - expected)
                        .with_seq_range(expected, first_retained - 1);
                let _ = sub.push_gap(gap);
            }
        }
        for s in &st.ring {
            if replay.wants(s.seq) {
                let _ = sub.push(
                    event_record(name, s.seq, s.ts_ns, &s.payload),
                    event_retained_bytes(name, s.retained_bytes),
                );
            }
        }
        st.subs.push(sub);
        Ok(EventReceiver { queue })
    }

    /// Block for the next payload on `name` (site/content/internals/streams-channels.md). `timeout` bounds the
    /// wait: `timeout`/`channel_closed` errors surface rather than blocking a host
    /// forever. Subscribes with no replay, so only events published *after* this
    /// call are seen.
    pub fn take(&self, name: &str, timeout: Option<Duration>) -> VResult<Value> {
        self.take_cancelled(name, timeout, None)
    }

    fn take_cancelled(
        &self,
        name: &str,
        timeout: Option<Duration>,
        cancel: Option<&CancelToken>,
    ) -> VResult<Value> {
        let rx = self.subscribe(name, Replay::None)?;
        let deadline = timeout
            .map(|duration| {
                Instant::now().checked_add(duration).ok_or_else(|| {
                    ErrorVal::arg_error(format!("channel `{name}` timeout is out of range"))
                })
            })
            .transpose()?;
        loop {
            let remaining = deadline.map(|end| end.saturating_duration_since(Instant::now()));
            match rx.recv(remaining, cancel) {
                Received::Event(event) => return Ok(payload_of(&event)),
                // `.take` promises a payload rather than a delivery-status
                // record. Skip the marker and return the oldest retained event.
                Received::Gap(_) => continue,
                Received::Timeout => {
                    return Err(ErrorVal::new(
                        "timeout",
                        format!("channel `{name}`: no event within timeout"),
                    ));
                }
                Received::Closed => {
                    return Err(ErrorVal::new(
                        "channel_closed",
                        format!("channel `{name}` closed"),
                    ));
                }
                Received::Cancelled => {
                    return Err(ErrorVal::new(
                        "cancelled",
                        format!("channel `{name}` wait cancelled"),
                    ));
                }
                Received::Poisoned => return Err(channel_poisoned("subscriber queue")),
            }
        }
    }

    fn forwarder_snapshot(&self) -> VResult<Option<SharedEventForwarder>> {
        if self.forwarder_quarantined.load(Ordering::Acquire) {
            return Err(channel_poisoned("event forwarder"));
        }
        match self.forwarder.lock() {
            Ok(forwarder) => Ok(forwarder.clone()),
            Err(poisoned) => {
                poisoned.into_inner().take();
                self.forwarder.clear_poison();
                self.forwarder_quarantined.store(true, Ordering::Release);
                Err(channel_poisoned("event forwarder"))
            }
        }
    }

    fn lock_channels(&self) -> VResult<MutexGuard<'_, HashMap<String, ChannelState>>> {
        if self.channels_quarantined.load(Ordering::Acquire) {
            return Err(channel_poisoned("channel registry"));
        }
        match self.channels.lock() {
            Ok(channels) => Ok(channels),
            Err(poisoned) => {
                self.quarantine_channels(poisoned);
                Err(channel_poisoned("channel registry"))
            }
        }
    }

    fn quarantine_channels(
        &self,
        poisoned: PoisonError<MutexGuard<'_, HashMap<String, ChannelState>>>,
    ) {
        self.channels_quarantined.store(true, Ordering::Release);
        let channels = poisoned.into_inner();
        self.quarantine_known_channels(&channels);
    }

    fn quarantine_known_channels(&self, channels: &HashMap<String, ChannelState>) {
        self.channels_quarantined.store(true, Ordering::Release);
        for channel in channels.values() {
            for subscriber in &channel.subs {
                subscriber.0.quarantine();
            }
        }
    }
}

fn validate_channel_name(name: &str) -> VResult<()> {
    if name.len() <= CHANNEL_NAME_BYTES {
        return Ok(());
    }
    Err(ErrorVal::new(
        "channel_name_limit",
        format!(
            "channel name is {} UTF-8 bytes; maximum is {CHANNEL_NAME_BYTES}",
            name.len()
        ),
    ))
}

fn admit_channel_identity(map: &HashMap<String, ChannelState>, name: &str) -> VResult<()> {
    if map.contains_key(name) || map.len() < CHANNEL_CAP {
        return Ok(());
    }
    Err(ErrorVal::new(
        "channel_registry_limit",
        format!(
            "channel identity limit ({CHANNEL_CAP}) reached; existing channel history is never evicted to admit a new name"
        ),
    ))
}

fn prune_closed_subscribers(map: &mut HashMap<String, ChannelState>) {
    for channel in map.values_mut() {
        channel.subs.retain(Subscriber::is_open);
    }
    // A subscribe/drop cycle with no published history has no identity or
    // sequence state to preserve. Removing only these empty shells prevents
    // subscriber churn from exhausting the channel-identity budget. Any
    // channel that has published retains its ring and monotonic sequence.
    map.retain(|_, channel| !channel.ring.is_empty() || !channel.subs.is_empty());
}

fn event_retained_bytes(name: &str, payload_bytes: usize) -> usize {
    payload_bytes
        .saturating_add(name.len())
        .saturating_add(EVENT_RECORD_OVERHEAD)
}

fn payload_retained_size(value: &Value) -> VResult<usize> {
    retained_size(value, CHANNEL_RETAINED_LIMITS).map_err(|error| match error {
        RetainedError::Opaque(kind) => ErrorVal::new(
            "channel_payload_type",
            format!(
                "a {kind} cannot be retained in a bounded channel payload; publish materialized data instead"
            ),
        ),
        RetainedError::Secret => ErrorVal::new(
            "channel_payload_type",
            "a secret cannot be retained in a channel payload",
        ),
        RetainedError::Bytes { measured, max } => ErrorVal::new(
            "channel_payload_limit",
            format!("channel payload retains {measured} bytes; maximum is {max}"),
        ),
        RetainedError::Depth { measured, max } => ErrorVal::new(
            "channel_payload_limit",
            format!("channel payload depth is {measured}; maximum is {max}"),
        ),
        RetainedError::Nodes { measured, max } => ErrorVal::new(
            "channel_payload_limit",
            format!("channel payload has {measured} nodes; maximum is {max}"),
        ),
        RetainedError::AccountingOverflow => ErrorVal::new(
            "channel_payload_limit",
            "channel payload retained-size accounting overflowed",
        ),
    })
}

fn channel_poisoned(component: &str) -> ErrorVal {
    ErrorVal::new(
        "channel_poisoned",
        format!("{component} state is unavailable after an internal panic; restart the session"),
    )
}

/// `{channel, seq, ts, payload}` — the wire event shape (site/content/internals/kernel-protocol.md),
/// yielded by `channel(name).events()`.
fn event_record(name: &str, seq: u64, ts_ns: i128, payload: &Value) -> Value {
    let mut r = Record::new();
    r.insert("channel".into(), Value::Str(name.to_string()));
    r.insert("seq".into(), Value::Int(seq as i64));
    r.insert("ts".into(), datetime_from_ns(ts_ns));
    r.insert("payload".into(), payload.clone());
    Value::Record(r)
}

/// In-band indication that this subscriber fell behind. The normal event keys
/// remain present for shape compatibility; `seq: null` means this is not a
/// published event, while `overflow` and `dropped` describe the local gap.
fn overflow_record(name: &str, gap: StreamGap) -> Value {
    let mut r = gap.into_record();
    r.insert("channel".into(), Value::Str(name.to_string()));
    r.insert("seq".into(), Value::Null);
    r.insert("ts".into(), datetime_from_ns(now_ns()));
    r.insert("payload".into(), Value::Null);
    r.insert("overflow".into(), Value::Bool(true));
    Value::Record(r)
}

fn payload_of(event: &Value) -> Value {
    match event {
        Value::Record(r) => r.get("payload").cloned().unwrap_or(Value::Null),
        other => other.clone(),
    }
}

fn event_seq(event: &Value) -> Option<u64> {
    let Value::Record(record) = event else {
        return None;
    };
    match record.get("seq") {
        Some(Value::Int(seq)) if *seq >= 0 => Some(*seq as u64),
        _ => None,
    }
}

/// The runtime value returned by `channel("x")`. shoal's `Value` enum is closed
/// to additive-only changes shared with the kernel wire, so a channel handle is
/// modeled as a one-field record `{channel: <name>}` rather than a new variant.
/// The evaluator intercepts `.emit/.events/.latest/.take` (and `.into(...)`) on
/// values of this shape before generic method dispatch.
pub(crate) fn channel_handle(name: &str) -> VResult<Value> {
    validate_channel_name(name)?;
    let mut r = Record::new();
    r.insert("channel".into(), Value::Str(name.to_string()));
    Ok(Value::Record(r))
}

/// Recognize a channel handle (see [`channel_handle`]) and return its name.
pub(crate) fn as_channel(v: &Value) -> Option<&str> {
    if let Value::Record(r) = v
        && r.len() == 1
        && let Some(Value::Str(name)) = r.get("channel")
    {
        return Some(name);
    }
    None
}

fn now_ns() -> i128 {
    jiff::Timestamp::now().as_nanosecond()
}

fn datetime_from_ns(ns: i128) -> Value {
    match jiff::Timestamp::from_nanosecond(ns) {
        Ok(ts) => Value::DateTime(Box::new(ts.to_zoned(jiff::tz::TimeZone::system()))),
        Err(_) => Value::Null,
    }
}

#[cfg(test)]
#[path = "channels/tests.rs"]
mod tests;
