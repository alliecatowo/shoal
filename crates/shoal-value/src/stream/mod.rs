//! `StreamVal` and the lazy stream combinators (docs/STREAMS.md), moved
//! verbatim out of `lib.rs`.
//!
//! The one substrate for time-varying data. A `stream<T>` is a **lazy**,
//! **single-consumption** (TDD §1.9), **pull-based** pipeline: a base source
//! (`watch`/`tail`/`every`/`channel().events()`/a list) wrapped in zero or more
//! lazy combinator stages (§3). No work happens — no closure runs, no OS
//! resource opens — until a sink (§4) drives it. Identity equality.
//!
//! Because closure-bearing stages (`.map`/`.where`/`.scan`/`.flat_map`) must call
//! back into the evaluator, driving requires a [`CallCtx`]; the whole pipeline is
//! therefore driven at the sink, which holds the ctx, rather than being a plain
//! `Iterator`.
//!
//! The lazy combinator stages themselves (`Map`/`Filter`/`Scan`/…) live in
//! [`ops`], split out for size.

mod ops;

use super::*;

#[derive(Clone)]
pub struct StreamVal {
    pub label: String,
    /// `false` for endless sources (`every`/`watch`/`tail`/a channel with no
    /// `.take`/`.take_until` bound). `.collect()` on an unbounded stream errors
    /// `stream_unbounded` (STREAMS §4) rather than looping forever.
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

    /// Take the composed upstream, enforcing single-consumption (TDD §1.9): a
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

    // --- lazy combinators (STREAMS §3) -----------------------------------

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
                seen: Vec::new(),
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
    pub fn buffer(self, _n: usize) -> VResult<StreamVal> {
        // Pure pacing decoupler: in a synchronous pull model it has no observable
        // effect on the item sequence, so it is an identity stage. It exists so
        // `.buffer(n)` type-checks and reads intentionally in a chain.
        Ok(self)
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
            Box::new(ops::Zip { a: up, b: other_up })
        })
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
/// source (STREAMS §4) — the caller must `.take`/`.take_until` first.
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
