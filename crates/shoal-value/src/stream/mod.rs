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

/// Machine-readable reason carried by every stream loss marker. The runtime
/// still represents markers as ordinary records at the language boundary, but
/// producers construct them through this closed enum so spellings and fields
/// cannot drift independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamGapReason {
    SubscriberOverflow,
    HistoryEvicted,
    MixedOverflow,
    TeeOverflow,
    TailOverflow,
    WatchOverflow,
}

impl StreamGapReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SubscriberOverflow => "subscriber_overflow",
            Self::HistoryEvicted => "history_evicted",
            Self::MixedOverflow => "mixed_overflow",
            Self::TeeOverflow => "tee_overflow",
            Self::TailOverflow => "tail_overflow",
            Self::WatchOverflow => "watch_overflow",
        }
    }
}

/// Typed internal representation of an in-band stream gap. `from_seq` and
/// `to_seq` are populated for sequenced channel events and remain `null` for
/// sources such as `tail` and `.tee` that have no public sequence space.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamGap {
    pub reason: StreamGapReason,
    pub dropped: u64,
    pub from_seq: Option<u64>,
    pub to_seq: Option<u64>,
}

impl StreamGap {
    pub fn new(reason: StreamGapReason, dropped: u64) -> Self {
        Self {
            reason,
            dropped,
            from_seq: None,
            to_seq: None,
        }
    }

    pub fn with_seq_range(mut self, from_seq: u64, to_seq: u64) -> Self {
        self.from_seq = Some(from_seq);
        self.to_seq = Some(to_seq);
        self
    }

    /// Merge an older gap into this one while retaining an exact loss count and
    /// the widest known sequence range.
    pub fn absorb(&mut self, other: StreamGap) {
        if self.reason != other.reason {
            self.reason = StreamGapReason::MixedOverflow;
        }
        self.dropped = self.dropped.saturating_add(other.dropped);
        self.from_seq = match (self.from_seq, other.from_seq) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.to_seq = match (self.to_seq, other.to_seq) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }

    /// Stable discriminated record shape exposed to Shoal code.
    pub fn into_record(self) -> Record {
        let mut r = Record::new();
        r.insert("marker".into(), Value::Str("stream_gap".into()));
        r.insert("reason".into(), Value::Str(self.reason.as_str().into()));
        r.insert(
            "dropped".into(),
            Value::Int(self.dropped.min(i64::MAX as u64) as i64),
        );
        r.insert(
            "from_seq".into(),
            self.from_seq
                .map_or(Value::Null, |n| Value::Int(n.min(i64::MAX as u64) as i64)),
        );
        r.insert(
            "to_seq".into(),
            self.to_seq
                .map_or(Value::Null, |n| Value::Int(n.min(i64::MAX as u64) as i64)),
        );
        r
    }

    pub fn into_value(self) -> Value {
        Value::Record(self.into_record())
    }
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
struct ChanSource {
    rx: std::sync::mpsc::Receiver<VResult<Value>>,
    stop: Option<Arc<std::sync::atomic::AtomicBool>>,
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}
impl Upstream for ChanSource {
    fn pull(
        &mut self,
        _ctx: &mut dyn CallCtx,
        timeout: Option<std::time::Duration>,
    ) -> VResult<Pull> {
        use std::sync::mpsc::RecvTimeoutError;
        match timeout {
            None => match self.rx.recv() {
                Ok(r) => r.map(Pull::Item),
                Err(_) => Ok(Pull::End),
            },
            Some(d) => match self.rx.recv_timeout(d) {
                Ok(r) => r.map(Pull::Item),
                Err(RecvTimeoutError::Timeout) => Ok(Pull::Timeout),
                Err(RecvTimeoutError::Disconnected) => Ok(Pull::End),
            },
        }
    }
}

impl Drop for ChanSource {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop {
            stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if let Some(on_drop) = self.on_drop.take() {
            on_drop();
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
        StreamVal::from_source(
            label,
            false,
            Box::new(ChanSource {
                rx,
                stop: None,
                on_drop: None,
            }),
        )
    }

    /// Build a channel-backed stream while preserving the producer's known
    /// natural boundedness. Used by evaluator-owned pumps such as `.buffer`.
    pub fn from_buffered_channel(
        label: impl Into<String>,
        bounded: bool,
        rx: std::sync::mpsc::Receiver<VResult<Value>>,
        stop: Arc<std::sync::atomic::AtomicBool>,
        on_drop: Box<dyn FnOnce() + Send>,
    ) -> StreamVal {
        StreamVal::from_source(
            label,
            bounded,
            Box::new(ChanSource {
                rx,
                stop: Some(stop),
                on_drop: Some(on_drop),
            }),
        )
    }

    /// Build a stream from an evaluator-owned source.
    ///
    /// This is the ownership seam for live sources whose receive or pump
    /// policy cannot be represented by `std::sync::mpsc` (for example a
    /// bounded event subscription with explicit overflow records). The source
    /// remains single-consumption and is driven through the normal `CallCtx`.
    pub fn from_upstream(
        label: impl Into<String>,
        bounded: bool,
        up: Box<dyn Upstream>,
    ) -> StreamVal {
        StreamVal::from_source(label, bounded, up)
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
            Box::new(ops::FlatMapSequential {
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
                prefer_a: true,
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
                pending_b: None,
                wait_a: true,
                done: false,
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
        fn buffer_stream(&mut self, _stream: StreamVal, _capacity: usize) -> VResult<StreamVal> {
            unreachable!("stream buffer is not exercised by this pull test context")
        }
        fn cwd(&self) -> PathBuf {
            std::env::temp_dir()
        }
        fn fs(&self) -> &dyn Fs {
            static STD: StdFs = StdFs;
            &STD
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

    #[test]
    fn stream_gap_has_a_stable_discriminator_and_merges_ranges() {
        let mut gap = StreamGap::new(StreamGapReason::HistoryEvicted, 3).with_seq_range(4, 6);
        gap.absorb(StreamGap::new(StreamGapReason::SubscriberOverflow, 2).with_seq_range(7, 8));
        assert_eq!(gap.reason, StreamGapReason::MixedOverflow);
        assert_eq!(gap.dropped, 5);
        assert_eq!(gap.from_seq, Some(4));
        assert_eq!(gap.to_seq, Some(8));
        let record = gap.into_record();
        assert_eq!(record.get("marker"), Some(&Value::Str("stream_gap".into())));
        assert_eq!(
            record.get("reason"),
            Some(&Value::Str("mixed_overflow".into()))
        );
        assert_eq!(record.get("dropped"), Some(&Value::Int(5)));
        assert_eq!(record.get("from_seq"), Some(&Value::Int(4)));
        assert_eq!(record.get("to_seq"), Some(&Value::Int(8)));
    }
}
