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

use crate::{ChildScope, Evaluator};
use shoal_ast::Args;
use shoal_exec::CancelToken;
use shoal_value::{CallArgs, ErrorVal, Record, StreamVal, TaskVal, VResult, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Ring depth per channel (site/content/internals/kernel-protocol.md requires ≥1024). Older history for a
/// user channel is evicted once past this — durable history is `.save(path)` or a
/// journaled channel, never an unbounded ring.
const RING_CAP: usize = 1024;

/// Which buffered events a new subscriber replays before going live.
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
    subs: Vec<Sender<VResult<Value>>>,
}

/// A host-installed hook mirroring in-language emits onto an external bus
/// (the kernel `EventBus`, so wire subscribers see them — site/content/internals/kernel-protocol.md
/// "one substrate" promise).
pub type EventForwarder = Box<dyn Fn(&str, &Value) + Send + Sync>;

/// Session-scoped, in-process event bus backing in-language channels.
#[derive(Default)]
pub struct EventBus {
    channels: Mutex<HashMap<String, ChannelState>>,
    /// Mirrors `user.*` emits to a hosting kernel's wire bus (see [`Self::emit`]).
    forwarder: Mutex<Option<EventForwarder>>,
}

impl EventBus {
    pub fn shared() -> Arc<EventBus> {
        Arc::new(EventBus::default())
    }

    /// Install the external forwarder (kernel hosting only; the standalone
    /// REPL/script binary never sets one and behaves exactly as before).
    pub fn set_forwarder(&self, f: EventForwarder) {
        *self.forwarder.lock().unwrap() = Some(f);
    }

    /// Publish `payload` on `name`; returns the assigned monotonic `seq`. Every
    /// live subscriber receives the event record; dead subscribers (their stream
    /// dropped) are pruned. `user.*` events are additionally mirrored to the
    /// host's external bus when a forwarder is installed — the SAME
    /// client-writable rule the wire's `events.publish` enforces, so language
    /// code can never spoof a kernel-owned semantic channel
    /// (`journal`/`approval`/`session.transcript`/…) to wire subscribers.
    pub fn emit(&self, name: &str, payload: Value) -> u64 {
        let seq = self.publish_local(name, &payload);
        if name.starts_with("user.")
            && let Some(f) = self.forwarder.lock().unwrap().as_ref()
        {
            f(name, &payload);
        }
        seq
    }

    /// Publish an event that ORIGINATED on the external bus (the reverse
    /// direction of [`Self::emit`]'s mirror): ring + local subscribers only,
    /// never the forwarder — that would echo the event straight back out.
    pub fn inject(&self, name: &str, payload: Value) -> u64 {
        self.publish_local(name, &payload)
    }

    fn publish_local(&self, name: &str, payload: &Value) -> u64 {
        let mut map = self.channels.lock().unwrap();
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
        st.subs.retain(|tx| tx.send(Ok(event.clone())).is_ok());
        seq
    }

    /// The last payload published on `name`, or `null` if none (no wait).
    pub fn latest(&self, name: &str) -> Value {
        let map = self.channels.lock().unwrap();
        map.get(name)
            .and_then(|st| st.ring.back())
            .map(|s| s.payload.clone())
            .unwrap_or(Value::Null)
    }

    /// Subscribe to `name`, returning a receiver of `event` records. Replay
    /// mirrors the kernel EventBus (site/content/internals/kernel-protocol.md): `since: None` replays the
    /// whole ring then goes live; `since: Some(n)` replays only `seq > n` (the
    /// in-language `?since=` cursor, site/content/internals/streams-channels.md), then live.
    pub fn events(&self, name: &str, since: Option<u64>) -> Receiver<VResult<Value>> {
        self.subscribe(name, Replay::from_since(since))
    }

    /// Register a subscriber with the given replay policy.
    fn subscribe(&self, name: &str, replay: Replay) -> Receiver<VResult<Value>> {
        let (tx, rx) = channel();
        let mut map = self.channels.lock().unwrap();
        let st = map.entry(name.to_string()).or_default();
        for s in &st.ring {
            if replay.wants(s.seq) {
                let _ = tx.send(Ok(event_record(name, s.seq, s.ts_ns, &s.payload)));
            }
        }
        st.subs.push(tx);
        rx
    }

    /// Block for the next payload on `name` (site/content/internals/streams-channels.md). `timeout` bounds the
    /// wait: `timeout`/`channel_closed` errors surface rather than blocking a host
    /// forever. Subscribes with no replay, so only events published *after* this
    /// call are seen.
    pub fn take(&self, name: &str, timeout: Option<Duration>) -> VResult<Value> {
        let rx = self.subscribe(name, Replay::None);
        let event = match timeout {
            Some(d) => rx.recv_timeout(d).map_err(|_| {
                ErrorVal::new(
                    "timeout",
                    format!("channel `{name}`: no event within timeout"),
                )
            })?,
            None => rx
                .recv()
                .map_err(|_| ErrorVal::new("channel_closed", format!("channel `{name}` closed")))?,
        };
        event.map(|e| payload_of(&e))
    }
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

fn payload_of(event: &Value) -> Value {
    match event {
        Value::Record(r) => r.get("payload").cloned().unwrap_or(Value::Null),
        other => other.clone(),
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

impl Evaluator {
    /// `channel(name).emit/.events/.latest/.take` (site/content/internals/streams-channels.md).
    pub(crate) fn eval_channel_method(
        &mut self,
        chan: &str,
        name: &str,
        args: CallArgs,
    ) -> VResult<Value> {
        let bus = self.bus();
        match name {
            "emit" => {
                let payload = args
                    .pos
                    .first()
                    .cloned()
                    .ok_or_else(|| ErrorVal::arg_error("emit expects a value to publish"))?;
                bus.emit(chan, payload);
                Ok(Value::Null)
            }
            "events" => {
                let since = match args.get_named("since").or_else(|| args.pos.first()) {
                    Some(Value::Int(s)) if *s >= 0 => Some(*s as u64),
                    None => None,
                    Some(v) => {
                        return Err(ErrorVal::type_error(format!(
                            "events `since` expects an int seq, found {}",
                            v.type_name()
                        )));
                    }
                };
                Ok(Value::Stream(StreamVal::from_channel(
                    "event",
                    bus.events(chan, since),
                )))
            }
            "latest" => Ok(bus.latest(chan)),
            "take" => {
                let timeout = match args.get_named("timeout").or_else(|| args.pos.first()) {
                    Some(Value::Duration(ns)) if *ns >= 0 => Some(Duration::from_nanos(*ns as u64)),
                    None => None,
                    Some(v) => {
                        return Err(ErrorVal::type_error(format!(
                            "take `timeout` expects a duration, found {}",
                            v.type_name()
                        )));
                    }
                };
                bus.take(chan, timeout)
            }
            _ => Err(ErrorVal::new(
                "field_missing",
                format!("unknown channel method `.{name}`"),
            )),
        }
    }

    /// Stream sinks needing the evaluator: `.into(channel(name))` republishes each
    /// item as an event; `.render()` drives the stream to the statement sink as a
    /// live view (site/content/internals/streams-channels.md). Both drive with `self` as the `CallCtx` directly
    /// (a manual pull loop) so each item can also reach an evaluator-only
    /// destination between pulls.
    pub(crate) fn eval_stream_sink(
        &mut self,
        recv: Value,
        name: &str,
        args: CallArgs,
    ) -> VResult<Value> {
        use shoal_value::Pull;
        let Value::Stream(s) = recv else {
            return Err(ErrorVal::type_error("stream sink on a non-stream"));
        };
        let target = if name == "into" {
            Some(
                args.pos
                    .first()
                    .and_then(as_channel)
                    .ok_or_else(|| {
                        ErrorVal::arg_error("into expects a channel: `.into(channel(\"name\"))`")
                    })?
                    .to_string(),
            )
        } else {
            None
        };
        let bus = self.bus();
        let mut up = s.take_upstream()?;
        loop {
            match up.pull(self, None)? {
                Pull::Item(v) => match &target {
                    Some(chan) => {
                        bus.emit(chan, v);
                    }
                    None => self.sink_value(&v),
                },
                Pull::End => break,
                Pull::Timeout => continue,
            }
        }
        Ok(Value::Null)
    }

    /// `on(channel(name) | name, handler)` (site/content/internals/streams-channels.md) — spawn a background task
    /// that runs `handler(event)` for every event on the channel. This is the
    /// in-language spelling of `spawn { channel(name).events().each(handler) }`
    /// (the bare `on channel(x){ev=>…}` keyword sugar needs a grammar change,
    /// which lives outside this crate). Returns the spawned `task`.
    pub(crate) fn builtin_on(&mut self, args: &Args) -> VResult<Value> {
        let a = self.eval_args(args)?;
        let chan = a
            .pos
            .first()
            .and_then(|v| match v {
                Value::Str(s) => Some(s.clone()),
                v => as_channel(v).map(str::to_string),
            })
            .ok_or_else(|| {
                ErrorVal::arg_error("on expects a channel (or channel name) then a handler")
            })?;
        let handler =
            a.pos.get(1).cloned().ok_or_else(|| {
                ErrorVal::arg_error("on expects a handler: `on(channel(\"x\"), f)`")
            })?;

        // Subscribe now (before spawning) so no event emitted between here and the
        // task starting is missed.
        let rx = self.bus().events(&chan, None);

        let task = TaskVal::new(format!("on channel({chan})"));
        // A FRESH cancel token wired to the task's cancel hook, so cancelling the
        // task interrupts the handler's exec tokens.
        let child_cancel = CancelToken::new();
        let hook_cancel = child_cancel.clone();
        task.on_cancel(Box::new(move || hook_cancel.cancel()));
        let worker = task.clone();
        // The one authoritative child constructor (HR-B1): the handler task runs
        // in a child that inherits the full session context — leash policy/
        // principal, reef state, config, all effect ports, the event bus, and
        // session identity. The old hand-copy shared only the ports and bus,
        // dropping leash/reef/config (audit B1–B4). `Inherit` scope: the handler
        // sees the caller's bindings.
        let ctx = self.child_context();
        std::thread::spawn(move || {
            let mut ev = ctx.build(ChildScope::Inherit, child_cancel);
            let stream = StreamVal::from_channel("event", rx);
            let result = match stream.take_upstream() {
                Ok(mut up) => shoal_value::drive_stream(&mut ev, &mut *up, |ctx, event| {
                    ctx.call_closure(&handler, vec![event]).map(|_| ())
                })
                .map(|()| Value::Null),
                Err(e) => Err(e),
            };
            worker.finish(result);
        });
        self.jobs.push(task.clone());
        Ok(Value::Task(task))
    }
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
