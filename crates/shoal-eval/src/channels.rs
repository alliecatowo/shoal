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
    CallArgs, CallCtx, ErrorVal, Pull, Record, StreamGap, StreamGapReason, StreamVal, TaskVal,
    Upstream, VResult, Value,
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

/// Per-subscriber live/replay capacity. Publishers never wait for a slow
/// subscriber: when this fills, the oldest queued deliveries are discarded and
/// represented by one in-band overflow record before the newest event.
const SUBSCRIBER_CAP: usize = 256;

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
}

#[derive(Default)]
struct ChannelState {
    next_seq: u64,
    ring: VecDeque<Stored>,
    subs: Vec<Subscriber>,
}

enum Delivery {
    Event(Value),
    Gap(StreamGap),
}

#[derive(Default)]
struct QueueState {
    items: VecDeque<Delivery>,
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
        state.items.push_back(Delivery::Gap(gap));
        self.0.ready.notify_one();
        true
    }

    fn push(&self, event: Value) -> bool {
        let mut state = self.lock_state();
        if state.closed {
            return false;
        }

        if state.items.len() >= SUBSCRIBER_CAP {
            // Reserve two tail slots: one exact gap marker and the newest
            // event. If an older marker is evicted, carry its count forward so
            // loss is never silently forgotten.
            let mut gap = StreamGap::new(StreamGapReason::SubscriberOverflow, 0);
            while state.items.len() > SUBSCRIBER_CAP.saturating_sub(2) {
                match state.items.pop_front() {
                    Some(Delivery::Event(event)) => {
                        let mut dropped = StreamGap::new(StreamGapReason::SubscriberOverflow, 1);
                        if let Some(seq) = event_seq(&event) {
                            dropped = dropped.with_seq_range(seq, seq);
                        }
                        gap.absorb(dropped);
                    }
                    Some(Delivery::Gap(dropped)) => gap.absorb(dropped),
                    None => break,
                }
            }
            state.items.push_back(Delivery::Gap(gap));
        }
        state.items.push_back(Delivery::Event(event));
        self.0.ready.notify_one();
        true
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
                return match item {
                    Delivery::Event(v) => Received::Event(v),
                    Delivery::Gap(gap) => Received::Gap(gap),
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
        // Resolve forwarder health before committing locally: a poisoned
        // bridge must not report failure after an event was already appended.
        let forwarder = if name.starts_with("user.") {
            self.forwarder_snapshot()?
        } else {
            None
        };
        let seq = self.publish_local(name, &payload)?;
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
        self.publish_local(name, &payload)
    }

    fn publish_local(&self, name: &str, payload: &Value) -> VResult<u64> {
        let mut map = self.lock_channels()?;
        if map
            .get(name)
            .is_some_and(|channel| channel.next_seq > i64::MAX as u64)
        {
            self.quarantine_known_channels(&map);
            return Err(channel_poisoned("channel sequence"));
        }
        let st = map.entry(name.to_string()).or_default();
        let seq = st.next_seq;
        st.next_seq += 1;
        let ts_ns = now_ns();
        st.ring.push_back(Stored {
            seq,
            ts_ns,
            payload: payload.clone(),
        });
        while st.ring.len() > RING_CAP {
            st.ring.pop_front();
        }
        let event = event_record(name, seq, ts_ns, payload);
        st.subs.retain(|sub| sub.push(event.clone()));
        Ok(seq)
    }

    /// The last payload published on `name`, or `null` if none (no wait).
    pub fn latest(&self, name: &str) -> VResult<Value> {
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
        self.subscribe(name, Replay::from_since(since))
    }

    /// Subscribe as a language stream. The custom upstream preserves the
    /// bounded queue's explicit overflow records instead of hiding it behind an
    /// unbounded `mpsc` adapter.
    pub fn event_stream(&self, name: &str, since: Option<u64>) -> VResult<StreamVal> {
        Ok(StreamVal::from_upstream(
            "event",
            false,
            Box::new(EventUpstream {
                channel: name.to_string(),
                rx: self.events(name, since)?,
            }),
        ))
    }

    /// Register a subscriber with the given replay policy.
    fn subscribe(&self, name: &str, replay: Replay) -> VResult<EventReceiver> {
        let queue = Arc::new(SubscriberQueue::default());
        let sub = Subscriber(queue.clone());
        let mut map = self.lock_channels()?;
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
                let _ = sub.push(event_record(name, s.seq, s.ts_ns, &s.payload));
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
pub(crate) fn channel_handle(name: &str) -> Value {
    let mut r = Record::new();
    r.insert("channel".into(), Value::Str(name.to_string()));
    Value::Record(r)
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
mod tests {
    use super::*;

    #[test]
    fn slow_subscriber_is_bounded_and_every_gap_is_accounted_for() {
        let bus = EventBus::default();
        let rx = bus.events("burst", None).unwrap();
        let published = SUBSCRIBER_CAP + 80;
        for i in 0..published {
            bus.emit("burst", Value::Int(i as i64)).unwrap();
        }

        let mut retained = 0usize;
        let mut dropped = 0usize;
        loop {
            match rx.recv(Some(Duration::ZERO), None) {
                Received::Event(_) => retained += 1,
                Received::Gap(gap) => dropped += gap.dropped as usize,
                Received::Timeout => break,
                Received::Closed | Received::Cancelled | Received::Poisoned => {
                    panic!("subscription ended early")
                }
            }
        }
        assert!(retained <= SUBSCRIBER_CAP);
        assert!(dropped > 0, "overflow must be explicit, never silent");
        assert_eq!(retained + dropped, published);
    }

    #[test]
    fn cancellation_wakes_an_idle_subscription_promptly() {
        let bus = EventBus::default();
        let rx = bus.events("idle", None).unwrap();
        let cancel = CancelToken::new();
        let trip = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            trip.cancel();
        });

        let start = Instant::now();
        assert!(matches!(rx.recv(None, Some(&cancel)), Received::Cancelled));
        assert!(
            start.elapsed() < Duration::from_millis(250),
            "cancelled receive stayed blocked"
        );
    }

    #[test]
    fn cancellation_preempts_a_subscription_backlog() {
        let bus = EventBus::default();
        let rx = bus.events("backlog", None).unwrap();
        for i in 0..SUBSCRIBER_CAP {
            bus.emit("backlog", Value::Int(i as i64)).unwrap();
        }
        let cancel = CancelToken::new();
        cancel.cancel();
        assert!(matches!(rx.recv(None, Some(&cancel)), Received::Cancelled));
    }

    #[test]
    fn stale_cursor_reports_exact_history_and_queue_gaps() {
        let bus = EventBus::default();
        let published = RING_CAP + 10;
        for i in 0..published {
            bus.emit("history", Value::Int(i as i64)).unwrap();
        }
        let rx = bus.events("history", Some(0)).unwrap();
        let mut retained = 0usize;
        let mut dropped = 0usize;
        let mut typed = false;
        loop {
            match rx.recv(Some(Duration::ZERO), None) {
                Received::Event(_) => retained += 1,
                Received::Gap(gap) => {
                    dropped += gap.dropped as usize;
                    let marker = overflow_record("history", gap);
                    let Value::Record(record) = marker else {
                        unreachable!()
                    };
                    typed |= record.get("marker") == Some(&Value::Str("stream_gap".into()))
                        && record.get("reason").is_some()
                        && record.get("from_seq").is_some()
                        && record.get("to_seq").is_some();
                }
                Received::Timeout => break,
                Received::Closed | Received::Cancelled | Received::Poisoned => {
                    panic!("subscription ended early")
                }
            }
        }
        assert!(typed, "every gap uses the stable discriminated shape");
        assert_eq!(
            retained + dropped,
            published - 1,
            "every event newer than the cursor is retained or accounted for"
        );
    }

    fn poison<T: Send>(mutex: &Mutex<T>) {
        std::thread::scope(|scope| {
            let poisoner = scope.spawn(|| {
                let _guard = mutex.lock().expect("test mutex should start healthy");
                panic!("inject evaluator channel poison");
            });
            assert!(poisoner.join().is_err());
        });
        assert!(mutex.is_poisoned());
    }

    #[test]
    fn poisoned_subscriber_queue_is_terminal_bounded_and_repeatable() {
        let bus = EventBus::default();
        let rx = bus.events("queue-poison", None).unwrap();
        for i in 0..SUBSCRIBER_CAP {
            bus.emit("queue-poison", Value::Int(i as i64)).unwrap();
        }
        poison(&rx.queue.state);

        assert!(matches!(
            rx.recv(Some(Duration::ZERO), None),
            Received::Poisoned
        ));
        assert!(matches!(
            rx.recv(Some(Duration::ZERO), None),
            Received::Poisoned
        ));
        let state = rx.queue.state.lock().expect("poison must be repaired");
        assert!(state.closed && state.poisoned);
        assert!(state.items.is_empty(), "unknown backlog must be discarded");
        assert!(state.items.capacity() <= SUBSCRIBER_CAP.next_power_of_two());
    }

    #[test]
    fn condvar_poison_wakes_and_terminalizes_a_blocked_waiter() {
        let bus = EventBus::default();
        let rx = bus.events("blocked-poison", None).unwrap();
        let queue = rx.queue.clone();
        let waiting = Arc::new(std::sync::Barrier::new(2));
        let waiter_barrier = waiting.clone();
        let waiter_queue = queue.clone();
        let waiter = std::thread::spawn(move || {
            let state = waiter_queue
                .state
                .lock()
                .expect("test queue should start healthy");
            waiter_barrier.wait();
            waiter_queue.wait(state).poisoned
        });
        waiting.wait();

        poison(&queue.state);
        queue.ready.notify_all();
        assert!(waiter.join().unwrap());
        assert!(matches!(rx.recv(None, None), Received::Poisoned));
    }

    #[test]
    fn poisoned_channel_registry_quarantines_repeated_calls_and_wakes_waiters() {
        let bus = Arc::new(EventBus::default());
        let rx = bus.events("registry-poison", None).unwrap();
        poison(&bus.channels);

        for error in [
            bus.emit("registry-poison", Value::Int(1)).unwrap_err(),
            bus.latest("registry-poison").unwrap_err(),
            bus.events("registry-poison", None).err().unwrap(),
        ] {
            assert_eq!(error.code, "channel_poisoned");
        }
        assert!(matches!(rx.recv(None, None), Received::Poisoned));
        assert!(matches!(rx.recv(None, None), Received::Poisoned));
        assert!(bus.channels_quarantined.load(Ordering::Acquire));
    }

    #[test]
    fn poisoned_forwarder_requires_explicit_replacement_before_publish() {
        let bus = EventBus::default();
        bus.set_forwarder(Box::new(|_, _| {}));
        poison(&bus.forwarder);

        for _ in 0..2 {
            let error = bus.emit("user.poison", Value::Int(1)).unwrap_err();
            assert_eq!(error.code, "channel_poisoned");
        }
        assert_eq!(bus.latest("user.poison").unwrap(), Value::Null);

        let forwarded = Arc::new(AtomicBool::new(false));
        let observed = forwarded.clone();
        bus.set_forwarder(Box::new(move |_, _| {
            observed.store(true, Ordering::Release);
        }));
        assert_eq!(bus.emit("user.poison", Value::Int(2)).unwrap(), 0);
        assert!(forwarded.load(Ordering::Acquire));
        assert_eq!(bus.latest("user.poison").unwrap(), Value::Int(2));
    }

    #[test]
    fn panicking_forwarder_is_contained_and_then_quarantined() {
        let bus = EventBus::default();
        bus.set_forwarder(Box::new(|_, _| panic!("inject forwarder panic")));
        let error = bus.emit("user.forwarder-panic", Value::Int(1)).unwrap_err();
        assert_eq!(error.code, "channel_poisoned");
        assert_eq!(
            bus.emit("user.forwarder-panic", Value::Int(2))
                .unwrap_err()
                .code,
            "channel_poisoned"
        );
        assert_eq!(
            bus.latest("user.forwarder-panic").unwrap(),
            Value::Int(1),
            "the committed local event remains authoritative"
        );
    }

    #[test]
    fn unrepresentable_sequence_quarantines_instead_of_wrapping() {
        let bus = EventBus::default();
        let rx = bus.events("seq-exhausted", None).unwrap();
        bus.channels
            .lock()
            .expect("test registry should be healthy")
            .get_mut("seq-exhausted")
            .expect("subscription creates channel")
            .next_seq = i64::MAX as u64 + 1;

        for _ in 0..2 {
            assert_eq!(
                bus.emit("seq-exhausted", Value::Null).unwrap_err().code,
                "channel_poisoned"
            );
        }
        assert!(matches!(rx.recv(None, None), Received::Poisoned));
    }

    #[test]
    fn production_channel_locks_have_no_raw_panicking_access() {
        let source = include_str!("channels.rs");
        let production = source
            .split("#[cfg(test)]")
            .next()
            .expect("production source prefix");
        for forbidden in [
            ".lock().unwrap(",
            ".lock().expect(",
            ".wait(state).unwrap(",
            ".wait_timeout(state, wait).unwrap(",
        ] {
            assert!(
                !production.contains(forbidden),
                "production channel synchronization contains `{forbidden}`"
            );
        }
        let evaluator_surface = include_str!("channels/eval.rs");
        let registered = evaluator_surface
            .find("self.exec.jobs.register(task.clone())")
            .expect("handler task registration");
        let launched = evaluator_surface[registered..]
            .find(".spawn(move ||")
            .expect("fallible handler launch")
            + registered;
        assert!(
            registered < launched,
            "handler task must be registered before its worker can run"
        );
    }
}
