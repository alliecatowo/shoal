//! `StreamVal` and the lazy stream combinators (site/content/internals/streams-channels.md), moved
//! verbatim out of `lib.rs`.
//!
//! The one substrate for time-varying data. A `stream<T>` is a **lazy**,
//! **single-consumption** (site/content/internals/language-conformance-contract.md), **pull-based** pipeline: a base source
//! (`watch`/`tail`/`every`/`channel().events()`/a list) wrapped in zero or more
//! lazy combinator stages (site/content/internals/streams-channels.md). No work happens — no closure runs, no OS
//! resource opens — until a sink (site/content/internals/streams-channels.md) drives it. Identity equality.
//!
//! Because closure-bearing stages (`.map`/`.where`/`.scan`/`.flat_map`) must call
//! back into the evaluator, driving requires a [`CallCtx`]; the whole pipeline is
//! therefore driven at the sink, which holds the ctx, rather than being a plain
//! `Iterator`.
//!
//! The lazy combinator stages themselves (`Map`/`Filter`/`Scan`/…) live in
//! [`ops`], split out for size.

mod ops;
mod tee;

use super::*;

#[derive(Clone)]
pub struct StreamVal {
    pub label: String,
    /// `false` for endless sources (`every`/`watch`/`tail`/a channel with no
    /// `.take`/`.take_until` bound). `.collect()` on an unbounded stream errors
    /// `stream_unbounded` (site/content/internals/streams-channels.md) rather than looping forever.
    bounded: bool,
    inner: Arc<Mutex<StreamState>>,
}

enum StreamState {
    Ready(Box<dyn Upstream>),
    Consumed,
}

/// One pull from an upstream, honoring an optional deadline.
pub enum Pull {
    Item(Value),
    /// The stream ended naturally.
    End,
    /// The deadline elapsed with no item (only ever produced by a live,
    /// channel-backed source or a timing combinator; an in-memory source never
    /// times out).
    Timeout,
}

/// A pull-based source or combinator stage. Closure-bearing stages receive the
/// evaluator through `ctx` at pull time.
pub trait Upstream: Send {
    fn pull(
        &mut self,
        ctx: &mut dyn CallCtx,
        timeout: Option<std::time::Duration>,
    ) -> VResult<Pull>;
}

/// Base source over an in-memory / lazy iterator (a list, range, `.tee` fork, or
/// a command's already-captured lines). Never times out.
struct IterSource(Box<dyn Iterator<Item = VResult<Value>> + Send>);
impl Upstream for IterSource {
    fn pull(
        &mut self,
        _ctx: &mut dyn CallCtx,
        _timeout: Option<std::time::Duration>,
    ) -> VResult<Pull> {
        match self.0.next() {
            Some(Ok(v)) => Ok(Pull::Item(v)),
            Some(Err(e)) => Err(e),
            None => Ok(Pull::End),
        }
    }
}

/// Base source over a live channel fed by a background producer (`every`'s timer,
/// `watch`/`tail`'s notify thread, a `channel().events()` subscription). Supports
/// timed reads so timing combinators (`debounce`/`throttle`) work.
struct ChanSource(std::sync::mpsc::Receiver<VResult<Value>>);
impl Upstream for ChanSource {
    fn pull(
        &mut self,
        _ctx: &mut dyn CallCtx,
        timeout: Option<std::time::Duration>,
    ) -> VResult<Pull> {
        use std::sync::mpsc::RecvTimeoutError;
        match timeout {
            None => match self.0.recv() {
                Ok(r) => r.map(Pull::Item),
                Err(_) => Ok(Pull::End),
            },
            Some(d) => match self.0.recv_timeout(d) {
                Ok(r) => r.map(Pull::Item),
                Err(RecvTimeoutError::Timeout) => Ok(Pull::Timeout),
                Err(RecvTimeoutError::Disconnected) => Ok(Pull::End),
            },
        }
    }
}

impl std::fmt::Debug for StreamVal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "stream<{}>", self.label)
    }
}

impl StreamVal {
    /// Build a stream from an in-memory / lazy iterator (a bounded source).
    pub fn from_iter<I>(label: impl Into<String>, iter: I) -> StreamVal
    where
        I: Iterator<Item = VResult<Value>> + Send + 'static,
    {
        StreamVal::from_source(label, true, Box::new(IterSource(Box::new(iter))))
    }

    /// Build a stream from a live channel fed by a background producer. Unbounded
    /// by default (an endless source) — bound it with `.take`/`.take_until` before
    /// `.collect()`.
    pub fn from_channel(
        label: impl Into<String>,
        rx: std::sync::mpsc::Receiver<VResult<Value>>,
    ) -> StreamVal {
        StreamVal::from_source(label, false, Box::new(ChanSource(rx)))
    }

    /// Like [`Self::from_channel`], but with an explicit boundedness bit — for
    /// thread-backed decoupling stages (`.buffer(n)`, HR-G1) that wrap an
    /// existing stream and must PRESERVE its boundedness: a buffered finite
    /// stream still has a natural end (`.collect()` stays legal), while a
    /// buffered live source stays endless.
    pub fn from_channel_bounded(
        label: impl Into<String>,
        bounded: bool,
        rx: std::sync::mpsc::Receiver<VResult<Value>>,
    ) -> StreamVal {
        StreamVal::from_source(label, bounded, Box::new(ChanSource(rx)))
    }

    fn from_source(label: impl Into<String>, bounded: bool, up: Box<dyn Upstream>) -> StreamVal {
        StreamVal {
            label: label.into(),
            bounded,
            inner: Arc::new(Mutex::new(StreamState::Ready(up))),
        }
    }

    /// Whether the stream has a natural end (used by `.collect()` to reject
    /// unbounded streams instead of looping forever).
    pub fn is_bounded(&self) -> bool {
        self.bounded
    }

    /// Take the composed upstream, enforcing single-consumption (site/content/internals/language-conformance-contract.md): a
    /// second attempt is `stream_consumed`.
    pub fn take_upstream(&self) -> VResult<Box<dyn Upstream>> {
        let mut g = self.inner.lock().unwrap();
        match std::mem::replace(&mut *g, StreamState::Consumed) {
            StreamState::Ready(up) => Ok(up),
            StreamState::Consumed => {
                Err(ErrorVal::new("stream_consumed", "stream already consumed")
                    .with_hint("collect first (`.collect()`), or `.tee(2)` to split"))
            }
        }
    }

    /// Consume `self` (single-consumption) and return a fresh stream whose
    /// upstream is `self`'s wrapped in a new stage. `bounded` is the new stream's
    /// boundedness.
    fn wrap(
        self,
        label: impl Into<String>,
        bounded: bool,
        make: impl FnOnce(Box<dyn Upstream>) -> Box<dyn Upstream>,
    ) -> VResult<StreamVal> {
        let up = self.take_upstream()?;
        Ok(StreamVal::from_source(label, bounded, make(up)))
    }

    pub fn same(&self, other: &StreamVal) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    // --- lazy combinators (site/content/internals/streams-channels.md) -----------------------------------

    pub fn map(self, f: Value) -> VResult<StreamVal> {
        let b = self.bounded;
        self.wrap("value", b, |up| Box::new(ops::Map { up, f }))
    }
    pub fn filter(self, f: Value) -> VResult<StreamVal> {
        let (b, l) = (self.bounded, self.label.clone());
        self.wrap(l, b, |up| Box::new(ops::Filter { up, f }))
    }
    pub fn scan(self, init: Value, f: Value) -> VResult<StreamVal> {
        let b = self.bounded;
        self.wrap("value", b, |up| Box::new(ops::Scan { up, f, acc: init }))
    }
    pub fn flat_map(self, f: Value) -> VResult<StreamVal> {
        let b = self.bounded;
        self.wrap("value", b, |up| {
            Box::new(ops::FlatMap {
                up,
                f,
                sub: None,
                queue: std::collections::VecDeque::new(),
            })
        })
    }
    pub fn take_n(self, n: usize) -> VResult<StreamVal> {
        let l = self.label.clone();
        // `.take` bounds any source — an endless stream becomes finite.
        self.wrap(l, true, |up| Box::new(ops::Take { up, remaining: n }))
    }
    pub fn take_until_pred(self, f: Value) -> VResult<StreamVal> {
        let (b, l) = (self.bounded, self.label.clone());
        self.wrap(l, b, |up| {
            Box::new(ops::TakeUntilPred { up, f, done: false })
        })
    }
    pub fn take_until_stream(self, other: StreamVal) -> VResult<StreamVal> {
        let (b, l) = (self.bounded, self.label.clone());
        let other_up = other.take_upstream()?;
        self.wrap(l, b, |up| {
            Box::new(ops::TakeUntilStream {
                up,
                other: other_up,
                done: false,
            })
        })
    }
    pub fn dedupe(self) -> VResult<StreamVal> {
        let (b, l) = (self.bounded, self.label.clone());
        self.wrap(l, b, |up| Box::new(ops::Dedupe { up, last: None }))
    }
    pub fn distinct(self) -> VResult<StreamVal> {
        let (b, l) = (self.bounded, self.label.clone());
        self.wrap(l, b, |up| {
            Box::new(ops::Distinct {
                up,
                seen: std::collections::HashMap::new(),
            })
        })
    }
    pub fn debounce(self, dur: std::time::Duration) -> VResult<StreamVal> {
        let (b, l) = (self.bounded, self.label.clone());
        self.wrap(l, b, |up| {
            Box::new(ops::Debounce {
                up,
                dur,
                pending: None,
                deadline: None,
            })
        })
    }
    pub fn throttle(self, dur: std::time::Duration) -> VResult<StreamVal> {
        let (b, l) = (self.bounded, self.label.clone());
        self.wrap(l, b, |up| {
            Box::new(ops::Throttle {
                up,
                dur,
                last: None,
            })
        })
    }
    pub fn window_count(self, n: usize) -> VResult<StreamVal> {
        let b = self.bounded;
        self.wrap("list", b, |up| {
            Box::new(ops::WindowCount {
                up,
                n,
                buf: std::collections::VecDeque::new(),
            })
        })
    }
    pub fn window_dur(self, dur: std::time::Duration) -> VResult<StreamVal> {
        let b = self.bounded;
        self.wrap("list", b, |up| {
            Box::new(ops::WindowDur {
                up,
                dur,
                buf: Vec::new(),
            })
        })
    }
    pub fn enumerate(self) -> VResult<StreamVal> {
        let b = self.bounded;
        self.wrap("list", b, |up| Box::new(ops::Enumerate { up, i: 0 }))
    }
    pub fn merge(self, other: StreamVal) -> VResult<StreamVal> {
        let bounded = self.bounded && other.bounded;
        let other_up = other.take_upstream()?;
        self.wrap("value", bounded, |up| {
            Box::new(ops::Merge {
                a: up,
                b: other_up,
                a_done: false,
                b_done: false,
            })
        })
    }
    pub fn zip(self, other: StreamVal) -> VResult<StreamVal> {
        // `.zip` ends when EITHER side ends, so a single bounded side bounds it.
        let bounded = self.bounded || other.bounded;
        let other_up = other.take_upstream()?;
        self.wrap("list", bounded, |up| {
            Box::new(ops::Zip {
                a: up,
                b: other_up,
                pending_a: None,
            })
        })
    }

    /// `.tee(n)` — fork into `n` independently-drivable streams sharing this
    /// stream's upstream (site/content/internals/streams-channels.md: "each replaying every item to its own
    /// sink"). Under the sync-pull model, whichever fork pulls next drives the
    /// shared source and the item is replayed into every sibling fork's
    /// BOUNDED queue ([`tee::TEE_QUEUE_CAP`]); a fork that falls further
    /// behind than the cap gets overflowed items coalesced into a
    /// `{dropped: n}` marker element (site/content/internals/streams-channels.md) instead of unbounded buffering.
    /// Forks inherit this stream's boundedness — a fork of an endless source
    /// is still endless (`.collect()` on it stays `stream_unbounded`).
    ///
    /// Bounded streams don't use this path: `methods/stream.rs` materializes
    /// them once and replays the full list per fork, preserving exact
    /// whole-stream replay with no cap.
    pub fn tee(self, n: usize) -> VResult<Vec<StreamVal>> {
        if n == 0 {
            return Err(ErrorVal::arg_error("tee count must be positive"));
        }
        let (label, bounded) = (self.label.clone(), self.bounded);
        let up = self.take_upstream()?;
        Ok(tee::fork(up, n)
            .into_iter()
            .map(|h| StreamVal::from_source(label.clone(), bounded, Box::new(h)))
            .collect())
    }
}

/// Drive a stream to a sink, invoking `on_item` for each produced value until the
/// stream ends. Blocks (no timeout) — the sink is the point where a live source
/// actually runs. Cancellation is by dropping the pipeline (which drops the base
/// receiver, so its producer thread exits).
pub fn drive_stream(
    ctx: &mut dyn CallCtx,
    up: &mut dyn Upstream,
    mut on_item: impl FnMut(&mut dyn CallCtx, Value) -> VResult<()>,
) -> VResult<()> {
    loop {
        match up.pull(ctx, None)? {
            Pull::Item(v) => on_item(ctx, v)?,
            Pull::End => return Ok(()),
            // A None-timeout pull never yields Timeout, but be total anyway.
            Pull::Timeout => continue,
        }
    }
}

/// Collect a bounded stream into a `Vec`. Errors `stream_unbounded` on an endless
/// source (site/content/internals/streams-channels.md) — the caller must `.take`/`.take_until` first.
pub fn collect_stream(ctx: &mut dyn CallCtx, s: &StreamVal) -> VResult<Vec<Value>> {
    if !s.bounded {
        return Err(
            ErrorVal::new("stream_unbounded", "this stream has no natural end")
                .with_hint("bound it first: `.take(n)` or `.take_until(...)`, or use `.each(f)`"),
        );
    }
    let mut up = s.take_upstream()?;
    let mut out = Vec::new();
    drive_stream(ctx, &mut *up, |_ctx, v| {
        out.push(v);
        Ok(())
    })?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct C;
    impl CallCtx for C {
        fn call_closure(&mut self, _f: &Value, _args: Vec<Value>) -> VResult<Value> {
            Err(ErrorVal::new("custom", "no closures in these tests"))
        }
        fn cwd(&self) -> PathBuf {
            std::env::temp_dir()
        }
    }

    /// An endless-MARKED in-memory source: exercises the live-fork tee path
    /// (bounded queues + drop/coalesce) deterministically, with no timers.
    fn endless_marked(n: i64) -> StreamVal {
        StreamVal::from_source(
            "int",
            false,
            Box::new(IterSource(Box::new((0..n).map(|i| Ok(Value::Int(i)))))),
        )
    }

    fn drain(s: &StreamVal) -> Vec<Value> {
        let mut up = s.take_upstream().unwrap();
        let mut out = Vec::new();
        drive_stream(&mut C, &mut *up, |_c, v| {
            out.push(v);
            Ok(())
        })
        .unwrap();
        out
    }

    #[test]
    fn zip_holds_the_left_item_across_a_right_timeout() {
        // HR-G6: zip pulls LEFT then RIGHT. When the right side times out, the
        // already-pulled left item must be HELD for the next pull — a timeout
        // is not the end of the stream, and discarding would be silent data
        // loss under timing combinators. The next successful pull pairs the
        // HELD item (1), not a freshly-pulled one (2).
        let (tx, rx) = std::sync::mpsc::channel();
        let a = StreamVal::from_iter("int", (1..=3).map(|i| Ok(Value::Int(i))));
        let b = StreamVal::from_channel("int", rx);
        let z = a.zip(b).unwrap();
        let mut up = z.take_upstream().unwrap();
        match up
            .pull(&mut C, Some(std::time::Duration::from_millis(30)))
            .unwrap()
        {
            Pull::Timeout => {}
            Pull::Item(v) => panic!("right side is silent; got item {v:?}"),
            Pull::End => panic!("stream is live; got End"),
        }
        tx.send(Ok(Value::Int(10))).unwrap();
        match up
            .pull(&mut C, Some(std::time::Duration::from_secs(5)))
            .unwrap()
        {
            Pull::Item(v) => assert_eq!(
                v,
                Value::List(vec![Value::Int(1), Value::Int(10)]),
                "the held left item pairs first — nothing was discarded"
            ),
            other => panic!(
                "expected the pair, got {:?}",
                matches!(other, Pull::End)
                    .then_some("End")
                    .unwrap_or("Timeout")
            ),
        }
    }

    #[test]
    fn merge_sweeps_left_first_but_never_starves_a_silent_side() {
        // HR-G6 precision: merge is a left-biased poll sweep, not fair
        // interleaving. (1) Two ready in-memory sources drain left side first.
        let a = StreamVal::from_iter("int", (1..=2).map(|i| Ok(Value::Int(i))));
        let b = StreamVal::from_iter("int", (3..=4).map(|i| Ok(Value::Int(i))));
        assert_eq!(
            drain(&a.merge(b).unwrap()),
            vec![Value::Int(1), Value::Int(2), Value::Int(3), Value::Int(4)],
            "ready sources drain left-first"
        );
        // (2) A silent-but-live left side only delays the right by one poll
        // step — the sweep moves on and delivers the ready right item.
        let (_keep_a_alive, rx_a) = std::sync::mpsc::channel();
        let (tx_b, rx_b) = std::sync::mpsc::channel();
        tx_b.send(Ok(Value::Int(7))).unwrap();
        let a = StreamVal::from_channel("int", rx_a);
        let b = StreamVal::from_channel("int", rx_b);
        let m = a.merge(b).unwrap();
        let mut up = m.take_upstream().unwrap();
        match up
            .pull(&mut C, Some(std::time::Duration::from_secs(5)))
            .unwrap()
        {
            Pull::Item(v) => assert_eq!(v, Value::Int(7), "right item flows past silent left"),
            _ => panic!("expected the right side's item"),
        }
    }

    #[test]
    fn tee_live_forks_replay_within_the_bound() {
        let forks = endless_marked(3).tee(2).unwrap();
        assert_eq!(forks.len(), 2);
        assert!(
            !forks[0].is_bounded(),
            "a fork of an endless stream is endless"
        );
        let a = drain(&forks[0]);
        let b = drain(&forks[1]);
        assert_eq!(a, vec![Value::Int(0), Value::Int(1), Value::Int(2)]);
        assert_eq!(b, a, "the second fork replays every item from its queue");
    }

    #[test]
    fn tee_live_fork_overflow_coalesces_to_dropped_marker() {
        // Fork 0 drains the whole 200-item source before fork 1 pulls once:
        // fork 1's bounded queue keeps the first TEE_QUEUE_CAP items; the
        // overflowed remainder is dropped and surfaced as one `{dropped: n}`
        // marker element — bounded memory with an honest signal (site/content/internals/streams-channels.md).
        let forks = endless_marked(200).tee(2).unwrap();
        let a = drain(&forks[0]);
        assert_eq!(a.len(), 200, "the pulling fork sees every item");
        let b = drain(&forks[1]);
        assert_eq!(b.len(), tee::TEE_QUEUE_CAP + 1);
        for (i, v) in b[..tee::TEE_QUEUE_CAP].iter().enumerate() {
            assert_eq!(v, &Value::Int(i as i64));
        }
        match &b[tee::TEE_QUEUE_CAP] {
            Value::Record(r) => assert_eq!(
                r.get("dropped"),
                Some(&Value::Int((200 - tee::TEE_QUEUE_CAP) as i64))
            ),
            other => panic!("expected a {{dropped: n}} marker, got {other:?}"),
        }
    }

    #[test]
    fn distinct_hash_buckets_respect_cross_type_equality() {
        // HR-G5: `distinct` now buckets by a canonical hash. The contract is
        // `a == b ⟹ same bucket`, so every cross-type equality `Value` defines
        // must still dedupe: Int/Float numeric promotion, Path/Str display
        // equality, and −0.0/+0.0. First occurrence wins, as before.
        let src = StreamVal::from_iter(
            "value",
            vec![
                Ok(Value::Int(1)),
                Ok(Value::Float(1.0)), // == Int(1)
                Ok(Value::Str("a".into())),
                Ok(Value::Path(PathBuf::from("a"))), // == Str("a")
                Ok(Value::Float(0.0)),
                Ok(Value::Float(-0.0)), // == 0.0
                Ok(Value::Int(2)),
            ]
            .into_iter(),
        );
        let out = drain(&src.distinct().unwrap());
        assert_eq!(
            out,
            vec![
                Value::Int(1),
                Value::Str("a".into()),
                Value::Float(0.0),
                Value::Int(2)
            ]
        );
    }

    #[test]
    fn distinct_emits_each_unique_value_once_at_scale() {
        // HR-G5: 10k items over 100 distinct keys — a smoke test that the
        // hash-bucketed set behaves exactly like the old linear scan.
        let src = StreamVal::from_iter("int", (0..10_000).map(|i| Ok(Value::Int(i % 100))));
        let out = drain(&src.distinct().unwrap());
        assert_eq!(out.len(), 100);
        for (i, v) in out.iter().enumerate() {
            assert_eq!(v, &Value::Int(i as i64), "first-occurrence order kept");
        }
    }

    #[test]
    fn tee_zero_is_an_error_and_consumed_stream_cannot_fork() {
        assert_eq!(
            endless_marked(1).tee(0).unwrap_err().code,
            "arg_error",
            "tee(0) is rejected"
        );
        let s = endless_marked(1);
        drain(&s);
        assert_eq!(s.tee(2).unwrap_err().code, "stream_consumed");
    }
}
