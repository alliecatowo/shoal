//! shoal-value — the runtime value model for the shoal shell.
//!
//! Types per TDD §4.1. `path` is bytes-backed (`PathBuf`/`OsString`); `secret`
//! is opaque; `outcome` is an external command's result; `stream` is
//! single-consumption; equality is structural for data types and identity for
//! `task`/`stream`.

pub mod methods;
pub mod ops;
pub mod ports;
pub mod render;

pub use ports::{Clock, Fs, Opener, SecretPort, StdClock, StdFs, StdOpener};

use indexmap::IndexMap;
use shoal_ast as ast;
use shoal_ast::Span;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

/// Ordered string-keyed record.
pub type Record = IndexMap<String, Value>;

/// Result type for all value-level operations.
pub type VResult<T> = Result<T, ErrorVal>;

// ---------------------------------------------------------------------------
// Value
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// Bytes-backed filesystem path (TDD §13.1).
    Path(PathBuf),
    Glob(GlobVal),
    Regex(Arc<RegexVal>),
    /// Bytes (decimal size literal), e.g. `1.5gb`.
    Size(u64),
    /// Nanoseconds, e.g. `250ms`.
    Duration(i64),
    DateTime(Box<jiff::Zoned>),
    Time(TimeVal),
    Bytes(Arc<Vec<u8>>),
    List(Vec<Value>),
    Record(Record),
    /// Semantically a `list<record>`; rendered as a table.
    Table(Vec<Record>),
    Range(RangeVal),
    Stream(StreamVal),
    Error(Arc<ErrorVal>),
    Outcome(Arc<OutcomeVal>),
    Task(TaskVal),
    Closure(Arc<ClosureVal>),
    /// Alias / partial command application (TDD §1.8).
    CmdRef(Arc<ast::CmdCall>),
    Secret(SecretVal),
}

impl Value {
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "str",
            Value::Path(_) => "path",
            Value::Glob(_) => "glob",
            Value::Regex(_) => "regex",
            Value::Size(_) => "size",
            Value::Duration(_) => "duration",
            Value::DateTime(_) => "datetime",
            Value::Time(_) => "time",
            Value::Bytes(_) => "bytes",
            Value::List(_) => "list",
            Value::Record(_) => "record",
            Value::Table(_) => "table",
            Value::Range(_) => "range",
            Value::Stream(_) => "stream",
            Value::Error(_) => "error",
            Value::Outcome(_) => "outcome",
            Value::Task(_) => "task",
            Value::Closure(_) => "closure",
            Value::CmdRef(_) => "command",
            Value::Secret(_) => "secret",
        }
    }

    /// Condition coercion (TDD §1.10): `bool` is itself; an `outcome`'s truth
    /// is its success. Everything else is a type error.
    pub fn as_condition(&self) -> VResult<bool> {
        match self {
            Value::Bool(b) => Ok(*b),
            Value::Outcome(o) => Ok(o.ok),
            other => Err(ErrorVal::new(
                "type_error",
                format!("expected bool in condition, found {}", other.type_name()),
            )
            .with_hint("shoal has no truthiness — try .is_empty(), .is_some(), or != null")),
        }
    }

    pub fn is_callable(&self) -> bool {
        matches!(self, Value::Closure(_) | Value::CmdRef(_))
    }
}

// ---------------------------------------------------------------------------
// Supporting payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct GlobVal {
    pub pattern: String,
    /// Origin cwd — expansion always happens against this (TDD §4.3).
    pub cwd: PathBuf,
    pub hidden: bool,
}

#[derive(Debug)]
pub struct RegexVal {
    pub src: String,
    pub re: regex::Regex,
}

impl RegexVal {
    pub fn compile(src: &str) -> VResult<RegexVal> {
        regex::Regex::new(src)
            .map(|re| RegexVal {
                src: src.to_string(),
                re,
            })
            .map_err(|e| ErrorVal::new("arg_error", format!("invalid regex: {e}")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeVal {
    pub hour: u8,
    pub min: u8,
    pub sec: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeVal {
    pub start: i64,
    pub end: i64,
    pub inclusive: bool,
}

impl RangeVal {
    pub fn iter(&self) -> impl Iterator<Item = i64> + Send + use<> {
        let (start, end, inclusive) = (self.start, self.end, self.inclusive);
        let last = if inclusive {
            end
        } else {
            end.saturating_sub(1)
        };
        start..=last
    }
    pub fn len(&self) -> usize {
        let last = if self.inclusive {
            self.end
        } else {
            self.end - 1
        };
        if last < self.start {
            0
        } else {
            (last - self.start + 1) as usize
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn contains(&self, v: i64) -> bool {
        v >= self.start
            && (if self.inclusive {
                v <= self.end
            } else {
                v < self.end
            })
    }
}

/// A command's result (TDD §4.1). `out` is parsed lazily on first structured
/// access; the raw bytes are always retained.
#[derive(Debug)]
pub struct OutcomeVal {
    pub status: Option<i32>,
    /// Signal name (`"SIGSEGV"`) when the child died to a signal (TDD §13.6).
    pub signal: Option<String>,
    pub ok: bool,
    pub stdout: Arc<Vec<u8>>,
    pub stderr: Arc<Vec<u8>>,
    pub dur_ns: i64,
    pub pid: u32,
    /// Display form of the invocation, for errors and rendering.
    pub cmd: String,
    pub parsed: Option<Value>,
    /// True only when the child's bytes actually reached the real terminal via
    /// the `ExecMode::PtyTee` passthrough path (defect #1). The interactive
    /// result renderer suppresses re-rendering exactly these outcomes to avoid
    /// double-printing; captured externals and builtins (which stream nothing)
    /// leave this `false` so their `.out` still renders.
    pub streamed: bool,
}

impl OutcomeVal {
    /// `outcome.out` — utf-8 text with the trailing newline trimmed; if the
    /// payload parses as JSON it becomes structured data (T1, lazy).
    pub fn out_value(&self) -> Value {
        if let Some(value) = &self.parsed {
            return value.clone();
        }
        let text = String::from_utf8_lossy(&self.stdout);
        let trimmed = text.strip_suffix('\n').unwrap_or(&text);
        let first = trimmed.trim_start().chars().next();
        if matches!(first, Some('{') | Some('['))
            && let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed)
        {
            return json_to_value(&json);
        }
        Value::Str(trimmed.to_string())
    }
}

pub fn json_to_value(j: &serde_json::Value) -> Value {
    match j {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else {
                Value::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_json::Value::String(s) => Value::Str(s.clone()),
        serde_json::Value::Array(xs) => {
            let vals: Vec<Value> = xs.iter().map(json_to_value).collect();
            // A uniform non-empty array of objects is a table.
            if !vals.is_empty() && vals.iter().all(|v| matches!(v, Value::Record(_))) {
                Value::Table(
                    vals.into_iter()
                        .map(|v| match v {
                            Value::Record(r) => r,
                            _ => unreachable!(),
                        })
                        .collect(),
                )
            } else {
                Value::List(vals)
            }
        }
        serde_json::Value::Object(m) => Value::Record(
            m.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        ),
    }
}

pub fn value_to_json(v: &Value) -> serde_json::Value {
    use serde_json::json;
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => json!(b),
        Value::Int(i) => json!(i),
        Value::Float(f) => json!(f),
        Value::Str(s) => json!(s),
        Value::Path(p) => json!(p.to_string_lossy()),
        Value::Glob(g) => json!(g.pattern),
        Value::Regex(r) => json!(r.src),
        Value::Size(n) => json!(n),
        Value::Duration(ns) => json!(ns),
        Value::DateTime(z) => json!(z.to_string()),
        Value::Time(t) => json!(render::render_time(t)),
        Value::Bytes(b) => json!(String::from_utf8_lossy(b)),
        Value::List(xs) => serde_json::Value::Array(xs.iter().map(value_to_json).collect()),
        Value::Record(r) => serde_json::Value::Object(
            r.iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect(),
        ),
        Value::Table(rows) => serde_json::Value::Array(
            rows.iter()
                .map(|r| {
                    serde_json::Value::Object(
                        r.iter()
                            .map(|(k, v)| (k.clone(), value_to_json(v)))
                            .collect(),
                    )
                })
                .collect(),
        ),
        Value::Range(r) => serde_json::Value::Array(r.iter().map(|i| json!(i)).collect()),
        Value::Outcome(o) => json!({
            "status": o.status, "ok": o.ok,
            "out": String::from_utf8_lossy(&o.stdout),
            "err": String::from_utf8_lossy(&o.stderr),
        }),
        Value::Error(e) => json!({"code": e.code, "msg": e.msg}),
        Value::Secret(s) => json!(format!("secret({})", s.name)),
        other => json!(render::render_inline(other)),
    }
}

/// Serialize a value to the bytes `.feed` writes to a command's stdin
/// (IO.md §1.2). The mapping is exhaustive over feedable types; anything else
/// is an error — `feed_error` (with a specialized message) for the types that
/// are *deliberately* never feedable, plain `type_error` otherwise.
pub fn feed_bytes(v: &Value) -> Result<Vec<u8>, ErrorVal> {
    match v {
        // str → its UTF-8 bytes, verbatim (no trailing newline added).
        Value::Str(s) => Ok(s.clone().into_bytes()),
        // bytes → raw.
        Value::Bytes(b) => Ok(b.as_ref().clone()),
        // path → its bytes.
        Value::Path(p) => Ok(p.to_string_lossy().into_owned().into_bytes()),
        // list<str> → each element joined with `\n`, plus one trailing `\n`.
        Value::List(xs) if xs.iter().all(|x| matches!(x, Value::Str(_))) => {
            let mut out = String::new();
            for x in xs {
                if let Value::Str(s) = x {
                    out.push_str(s);
                    out.push('\n');
                }
            }
            Ok(out.into_bytes())
        }
        // table / record / list-of-records (and any other list) → compact JSON.
        Value::Record(_) | Value::Table(_) | Value::List(_) => {
            Ok(serde_json::to_vec(&value_to_json(v)).unwrap_or_default())
        }
        // Deliberately never feedable (IO.md §1.2 / §5) → `feed_error`.
        Value::Secret(_) => Err(ErrorVal::new(
            "feed_error",
            "a secret cannot be fed as stdin data",
        )
        .with_hint("secrets are injected at spawn time, not fed as data")),
        Value::Task(_) | Value::Closure(_) | Value::Error(_) | Value::Glob(_) | Value::Regex(_) => {
            Err(ErrorVal::new(
                "feed_error",
                format!("a {} cannot be fed as stdin data", v.type_name()),
            ))
        }
        // Anything else has no pinned serialization yet → generic type_error.
        other => Err(ErrorVal::type_error(format!(
            "cannot feed a {} to a command's stdin",
            other.type_name()
        ))),
    }
}

/// The one substrate for time-varying data (docs/STREAMS.md). A `stream<T>` is a
/// **lazy**, **single-consumption** (TDD §1.9), **pull-based** pipeline: a base
/// source (`watch`/`tail`/`every`/`channel().events()`/a list) wrapped in zero or
/// more lazy combinator stages (§3). No work happens — no closure runs, no OS
/// resource opens — until a sink (§4) drives it. Identity equality.
///
/// Because closure-bearing stages (`.map`/`.where`/`.scan`/`.flat_map`) must call
/// back into the evaluator, driving requires a [`CallCtx`]; the whole pipeline is
/// therefore driven at the sink, which holds the ctx, rather than being a plain
/// `Iterator`.
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
        self.wrap("value", b, |up| Box::new(stream_ops::Map { up, f }))
    }
    pub fn filter(self, f: Value) -> VResult<StreamVal> {
        let (b, l) = (self.bounded, self.label.clone());
        self.wrap(l, b, |up| Box::new(stream_ops::Filter { up, f }))
    }
    pub fn scan(self, init: Value, f: Value) -> VResult<StreamVal> {
        let b = self.bounded;
        self.wrap("value", b, |up| {
            Box::new(stream_ops::Scan { up, f, acc: init })
        })
    }
    pub fn flat_map(self, f: Value) -> VResult<StreamVal> {
        let b = self.bounded;
        self.wrap("value", b, |up| {
            Box::new(stream_ops::FlatMap {
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
        self.wrap(l, true, |up| {
            Box::new(stream_ops::Take { up, remaining: n })
        })
    }
    pub fn take_until_pred(self, f: Value) -> VResult<StreamVal> {
        let (b, l) = (self.bounded, self.label.clone());
        self.wrap(l, b, |up| {
            Box::new(stream_ops::TakeUntilPred { up, f, done: false })
        })
    }
    pub fn take_until_stream(self, other: StreamVal) -> VResult<StreamVal> {
        let (b, l) = (self.bounded, self.label.clone());
        let other_up = other.take_upstream()?;
        self.wrap(l, b, |up| {
            Box::new(stream_ops::TakeUntilStream {
                up,
                other: other_up,
                done: false,
            })
        })
    }
    pub fn dedupe(self) -> VResult<StreamVal> {
        let (b, l) = (self.bounded, self.label.clone());
        self.wrap(l, b, |up| Box::new(stream_ops::Dedupe { up, last: None }))
    }
    pub fn distinct(self) -> VResult<StreamVal> {
        let (b, l) = (self.bounded, self.label.clone());
        self.wrap(l, b, |up| {
            Box::new(stream_ops::Distinct {
                up,
                seen: Vec::new(),
            })
        })
    }
    pub fn debounce(self, dur: std::time::Duration) -> VResult<StreamVal> {
        let (b, l) = (self.bounded, self.label.clone());
        self.wrap(l, b, |up| {
            Box::new(stream_ops::Debounce {
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
            Box::new(stream_ops::Throttle {
                up,
                dur,
                last: None,
            })
        })
    }
    pub fn window_count(self, n: usize) -> VResult<StreamVal> {
        let b = self.bounded;
        self.wrap("list", b, |up| {
            Box::new(stream_ops::WindowCount {
                up,
                n,
                buf: std::collections::VecDeque::new(),
            })
        })
    }
    pub fn window_dur(self, dur: std::time::Duration) -> VResult<StreamVal> {
        let b = self.bounded;
        self.wrap("list", b, |up| {
            Box::new(stream_ops::WindowDur {
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
        self.wrap("list", b, |up| Box::new(stream_ops::Enumerate { up, i: 0 }))
    }
    pub fn merge(self, other: StreamVal) -> VResult<StreamVal> {
        let bounded = self.bounded && other.bounded;
        let other_up = other.take_upstream()?;
        self.wrap("value", bounded, |up| {
            Box::new(stream_ops::Merge {
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
            Box::new(stream_ops::Zip { a: up, b: other_up })
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

/// The lazy combinator stages (STREAMS §3). Each wraps an inner [`Upstream`] and
/// is itself an [`Upstream`], so a chain composes by nesting.
mod stream_ops {
    use super::{CallCtx, Pull, Upstream, VResult, Value};
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    /// A short poll interval for stages that must interleave/observe two sources
    /// (`merge`, `take_until(stream)`) without a blocking read that could starve
    /// the other side.
    const POLL: Duration = Duration::from_millis(20);

    pub struct Map {
        pub up: Box<dyn Upstream>,
        pub f: Value,
    }
    impl Upstream for Map {
        fn pull(&mut self, ctx: &mut dyn CallCtx, t: Option<Duration>) -> VResult<Pull> {
            match self.up.pull(ctx, t)? {
                Pull::Item(v) => Ok(Pull::Item(ctx.call_closure(&self.f, vec![v])?)),
                other => Ok(other),
            }
        }
    }

    pub struct Filter {
        pub up: Box<dyn Upstream>,
        pub f: Value,
    }
    impl Upstream for Filter {
        fn pull(&mut self, ctx: &mut dyn CallCtx, t: Option<Duration>) -> VResult<Pull> {
            loop {
                match self.up.pull(ctx, t)? {
                    Pull::Item(v) => {
                        if ctx.call_closure(&self.f, vec![v.clone()])?.as_condition()? {
                            return Ok(Pull::Item(v));
                        }
                    }
                    other => return Ok(other),
                }
            }
        }
    }

    pub struct Scan {
        pub up: Box<dyn Upstream>,
        pub f: Value,
        pub acc: Value,
    }
    impl Upstream for Scan {
        fn pull(&mut self, ctx: &mut dyn CallCtx, t: Option<Duration>) -> VResult<Pull> {
            match self.up.pull(ctx, t)? {
                Pull::Item(v) => {
                    self.acc = ctx.call_closure(&self.f, vec![self.acc.clone(), v])?;
                    Ok(Pull::Item(self.acc.clone()))
                }
                other => Ok(other),
            }
        }
    }

    pub struct FlatMap {
        pub up: Box<dyn Upstream>,
        pub f: Value,
        pub sub: Option<Box<dyn Upstream>>,
        pub queue: VecDeque<Value>,
    }
    impl Upstream for FlatMap {
        fn pull(&mut self, ctx: &mut dyn CallCtx, t: Option<Duration>) -> VResult<Pull> {
            loop {
                if let Some(v) = self.queue.pop_front() {
                    return Ok(Pull::Item(v));
                }
                if let Some(sub) = self.sub.as_mut() {
                    match sub.pull(ctx, t)? {
                        Pull::Item(v) => return Ok(Pull::Item(v)),
                        Pull::End => self.sub = None,
                        Pull::Timeout => return Ok(Pull::Timeout),
                    }
                    continue;
                }
                match self.up.pull(ctx, t)? {
                    Pull::Item(v) => {
                        let r = ctx.call_closure(&self.f, vec![v])?;
                        match r {
                            Value::Stream(s) => self.sub = Some(s.take_upstream()?),
                            Value::List(xs) => self.queue.extend(xs),
                            Value::Table(rows) => {
                                self.queue.extend(rows.into_iter().map(Value::Record));
                            }
                            Value::Range(rg) => self.queue.extend(rg.iter().map(Value::Int)),
                            other => {
                                return Err(super::ErrorVal::type_error(format!(
                                    "flat_map expects each result to be a stream or list, found {}",
                                    other.type_name()
                                )));
                            }
                        }
                    }
                    other => return Ok(other),
                }
            }
        }
    }

    pub struct Take {
        pub up: Box<dyn Upstream>,
        pub remaining: usize,
    }
    impl Upstream for Take {
        fn pull(&mut self, ctx: &mut dyn CallCtx, t: Option<Duration>) -> VResult<Pull> {
            if self.remaining == 0 {
                return Ok(Pull::End);
            }
            match self.up.pull(ctx, t)? {
                Pull::Item(v) => {
                    self.remaining -= 1;
                    Ok(Pull::Item(v))
                }
                other => Ok(other),
            }
        }
    }

    pub struct TakeUntilPred {
        pub up: Box<dyn Upstream>,
        pub f: Value,
        pub done: bool,
    }
    impl Upstream for TakeUntilPred {
        fn pull(&mut self, ctx: &mut dyn CallCtx, t: Option<Duration>) -> VResult<Pull> {
            if self.done {
                return Ok(Pull::End);
            }
            match self.up.pull(ctx, t)? {
                Pull::Item(v) => {
                    if ctx.call_closure(&self.f, vec![v.clone()])?.as_condition()? {
                        self.done = true;
                        Ok(Pull::End)
                    } else {
                        Ok(Pull::Item(v))
                    }
                }
                other => Ok(other),
            }
        }
    }

    pub struct TakeUntilStream {
        pub up: Box<dyn Upstream>,
        pub other: Box<dyn Upstream>,
        pub done: bool,
    }
    impl Upstream for TakeUntilStream {
        fn pull(&mut self, ctx: &mut dyn CallCtx, t: Option<Duration>) -> VResult<Pull> {
            if self.done {
                return Ok(Pull::End);
            }
            let deadline = t.map(|d| Instant::now() + d);
            loop {
                // Has the signal stream produced anything yet? Non-blocking check.
                match self.other.pull(ctx, Some(Duration::ZERO))? {
                    Pull::Item(_) => {
                        self.done = true;
                        return Ok(Pull::End);
                    }
                    Pull::End | Pull::Timeout => {}
                }
                let step = match deadline {
                    Some(dl) => dl.saturating_duration_since(Instant::now()).min(POLL),
                    None => POLL,
                };
                match self.up.pull(ctx, Some(step))? {
                    Pull::Item(v) => return Ok(Pull::Item(v)),
                    Pull::End => return Ok(Pull::End),
                    Pull::Timeout => {
                        if deadline.is_some_and(|dl| Instant::now() >= dl) {
                            return Ok(Pull::Timeout);
                        }
                    }
                }
            }
        }
    }

    pub struct Dedupe {
        pub up: Box<dyn Upstream>,
        pub last: Option<Value>,
    }
    impl Upstream for Dedupe {
        fn pull(&mut self, ctx: &mut dyn CallCtx, t: Option<Duration>) -> VResult<Pull> {
            loop {
                match self.up.pull(ctx, t)? {
                    Pull::Item(v) => {
                        if self.last.as_ref() == Some(&v) {
                            continue;
                        }
                        self.last = Some(v.clone());
                        return Ok(Pull::Item(v));
                    }
                    other => return Ok(other),
                }
            }
        }
    }

    pub struct Distinct {
        pub up: Box<dyn Upstream>,
        pub seen: Vec<Value>,
    }
    impl Upstream for Distinct {
        fn pull(&mut self, ctx: &mut dyn CallCtx, t: Option<Duration>) -> VResult<Pull> {
            loop {
                match self.up.pull(ctx, t)? {
                    Pull::Item(v) => {
                        if self.seen.contains(&v) {
                            continue;
                        }
                        self.seen.push(v.clone());
                        return Ok(Pull::Item(v));
                    }
                    other => return Ok(other),
                }
            }
        }
    }

    pub struct Debounce {
        pub up: Box<dyn Upstream>,
        pub dur: Duration,
        pub pending: Option<Value>,
        pub deadline: Option<Instant>,
    }
    impl Upstream for Debounce {
        fn pull(&mut self, ctx: &mut dyn CallCtx, _t: Option<Duration>) -> VResult<Pull> {
            loop {
                let wait = self
                    .deadline
                    .map(|dl| dl.saturating_duration_since(Instant::now()));
                if let (Some(_), Some(w)) = (&self.pending, wait)
                    && w.is_zero()
                {
                    self.deadline = None;
                    return Ok(Pull::Item(self.pending.take().expect("pending")));
                }
                match self.up.pull(ctx, wait)? {
                    Pull::Item(v) => {
                        self.pending = Some(v);
                        self.deadline = Some(Instant::now() + self.dur);
                    }
                    Pull::Timeout => {
                        if let Some(v) = self.pending.take() {
                            self.deadline = None;
                            return Ok(Pull::Item(v));
                        }
                    }
                    Pull::End => {
                        return Ok(match self.pending.take() {
                            Some(v) => Pull::Item(v),
                            None => Pull::End,
                        });
                    }
                }
            }
        }
    }

    pub struct Throttle {
        pub up: Box<dyn Upstream>,
        pub dur: Duration,
        pub last: Option<Instant>,
    }
    impl Upstream for Throttle {
        fn pull(&mut self, ctx: &mut dyn CallCtx, t: Option<Duration>) -> VResult<Pull> {
            loop {
                match self.up.pull(ctx, t)? {
                    Pull::Item(v) => {
                        let now = Instant::now();
                        let emit = self.last.is_none_or(|l| now.duration_since(l) >= self.dur);
                        if emit {
                            self.last = Some(now);
                            return Ok(Pull::Item(v));
                        }
                    }
                    other => return Ok(other),
                }
            }
        }
    }

    pub struct WindowCount {
        pub up: Box<dyn Upstream>,
        pub n: usize,
        pub buf: VecDeque<Value>,
    }
    impl Upstream for WindowCount {
        fn pull(&mut self, ctx: &mut dyn CallCtx, t: Option<Duration>) -> VResult<Pull> {
            loop {
                match self.up.pull(ctx, t)? {
                    Pull::Item(v) => {
                        self.buf.push_back(v);
                        while self.buf.len() > self.n {
                            self.buf.pop_front();
                        }
                        if self.buf.len() == self.n {
                            return Ok(Pull::Item(Value::List(self.buf.iter().cloned().collect())));
                        }
                    }
                    other => return Ok(other),
                }
            }
        }
    }

    pub struct WindowDur {
        pub up: Box<dyn Upstream>,
        pub dur: Duration,
        pub buf: Vec<(Instant, Value)>,
    }
    impl Upstream for WindowDur {
        fn pull(&mut self, ctx: &mut dyn CallCtx, t: Option<Duration>) -> VResult<Pull> {
            match self.up.pull(ctx, t)? {
                Pull::Item(v) => {
                    let now = Instant::now();
                    self.buf.push((now, v));
                    let dur = self.dur;
                    self.buf.retain(|(ts, _)| now.duration_since(*ts) <= dur);
                    Ok(Pull::Item(Value::List(
                        self.buf.iter().map(|(_, v)| v.clone()).collect(),
                    )))
                }
                other => Ok(other),
            }
        }
    }

    pub struct Enumerate {
        pub up: Box<dyn Upstream>,
        pub i: i64,
    }
    impl Upstream for Enumerate {
        fn pull(&mut self, ctx: &mut dyn CallCtx, t: Option<Duration>) -> VResult<Pull> {
            match self.up.pull(ctx, t)? {
                Pull::Item(v) => {
                    let idx = self.i;
                    self.i += 1;
                    Ok(Pull::Item(Value::List(vec![Value::Int(idx), v])))
                }
                other => Ok(other),
            }
        }
    }

    pub struct Merge {
        pub a: Box<dyn Upstream>,
        pub b: Box<dyn Upstream>,
        pub a_done: bool,
        pub b_done: bool,
    }
    impl Upstream for Merge {
        fn pull(&mut self, ctx: &mut dyn CallCtx, t: Option<Duration>) -> VResult<Pull> {
            let deadline = t.map(|d| Instant::now() + d);
            loop {
                if self.a_done && self.b_done {
                    return Ok(Pull::End);
                }
                if !self.a_done {
                    match self.a.pull(ctx, Some(POLL))? {
                        Pull::Item(v) => return Ok(Pull::Item(v)),
                        Pull::End => self.a_done = true,
                        Pull::Timeout => {}
                    }
                }
                if !self.b_done {
                    match self.b.pull(ctx, Some(POLL))? {
                        Pull::Item(v) => return Ok(Pull::Item(v)),
                        Pull::End => self.b_done = true,
                        Pull::Timeout => {}
                    }
                }
                if deadline.is_some_and(|dl| Instant::now() >= dl) {
                    return Ok(Pull::Timeout);
                }
            }
        }
    }

    pub struct Zip {
        pub a: Box<dyn Upstream>,
        pub b: Box<dyn Upstream>,
    }
    impl Upstream for Zip {
        fn pull(&mut self, ctx: &mut dyn CallCtx, t: Option<Duration>) -> VResult<Pull> {
            let va = match self.a.pull(ctx, t)? {
                Pull::Item(v) => v,
                other => return Ok(other),
            };
            let vb = match self.b.pull(ctx, t)? {
                Pull::Item(v) => v,
                other => return Ok(other),
            };
            Ok(Pull::Item(Value::List(vec![va, vb])))
        }
    }
}

// ---------------------------------------------------------------------------
// Tasks
// ---------------------------------------------------------------------------

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct TaskVal {
    pub id: u64,
    pub shared: Arc<TaskShared>,
}

pub struct TaskShared {
    pub desc: String,
    state: Mutex<TaskState>,
    cond: Condvar,
    cancel_requested: AtomicBool,
    /// Hooks run on cancel (e.g. cancel exec tokens of children).
    on_cancel: Mutex<Vec<Box<dyn Fn() + Send>>>,
    /// Suspend state (TDD §4.7 job control): `task.suspend()` SIGTSTPs the task's
    /// process group, `task.resume()` SIGCONTs it. The actual OS signal is sent
    /// by hooks a spawner/host registers (`on_suspend`/`on_resume`), so this
    /// mechanism is signalling-backend-agnostic (a thread-only task simply has no
    /// hooks). `suspended` tracks the flag for `jobs`/prompt accounting.
    suspended: AtomicBool,
    on_suspend: Mutex<Vec<Box<dyn Fn() + Send>>>,
    on_resume: Mutex<Vec<Box<dyn Fn() + Send>>>,
}

impl std::fmt::Debug for TaskShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TaskShared({})", self.desc)
    }
}

enum TaskState {
    Running,
    Done(VResult<Value>),
}

impl TaskVal {
    pub fn new(desc: impl Into<String>) -> TaskVal {
        TaskVal {
            id: NEXT_TASK_ID.fetch_add(1, Ordering::Relaxed),
            shared: Arc::new(TaskShared {
                desc: desc.into(),
                state: Mutex::new(TaskState::Running),
                cond: Condvar::new(),
                cancel_requested: AtomicBool::new(false),
                on_cancel: Mutex::new(Vec::new()),
                suspended: AtomicBool::new(false),
                on_suspend: Mutex::new(Vec::new()),
                on_resume: Mutex::new(Vec::new()),
            }),
        }
    }

    pub fn finish(&self, result: VResult<Value>) {
        let mut g = self.shared.state.lock().unwrap();
        *g = TaskState::Done(result);
        self.shared.cond.notify_all();
    }

    pub fn wait(&self) -> VResult<Value> {
        let mut g = self.shared.state.lock().unwrap();
        loop {
            match &*g {
                TaskState::Done(r) => return r.clone(),
                TaskState::Running => g = self.shared.cond.wait(g).unwrap(),
            }
        }
    }

    pub fn is_done(&self) -> bool {
        matches!(&*self.shared.state.lock().unwrap(), TaskState::Done(_))
    }

    pub fn cancel(&self) {
        self.shared.cancel_requested.store(true, Ordering::SeqCst);
        for hook in self.shared.on_cancel.lock().unwrap().iter() {
            hook();
        }
    }

    pub fn cancel_requested(&self) -> bool {
        self.shared.cancel_requested.load(Ordering::SeqCst)
    }

    pub fn on_cancel(&self, hook: Box<dyn Fn() + Send>) {
        if self.cancel_requested() {
            hook();
        } else {
            self.shared.on_cancel.lock().unwrap().push(hook);
        }
    }

    /// Request the task suspend (TDD §4.7): mark it suspended and run every
    /// registered suspend hook (which is where a spawner/host sends `SIGTSTP` to
    /// the task's process group). Idempotent — suspending an already-suspended
    /// task re-runs the hooks, which is harmless (`SIGTSTP` to a stopped group is
    /// a no-op).
    pub fn suspend(&self) {
        self.shared.suspended.store(true, Ordering::SeqCst);
        for hook in self.shared.on_suspend.lock().unwrap().iter() {
            hook();
        }
    }

    /// Request the task resume (TDD §4.7): clear the suspended flag and run every
    /// registered resume hook (`SIGCONT` to the process group). Idempotent.
    pub fn resume(&self) {
        self.shared.suspended.store(false, Ordering::SeqCst);
        for hook in self.shared.on_resume.lock().unwrap().iter() {
            hook();
        }
    }

    pub fn is_suspended(&self) -> bool {
        self.shared.suspended.load(Ordering::SeqCst)
    }

    /// Register a hook run when the task is suspended (e.g. `SIGTSTP` the child
    /// process group). If the task is already suspended, the hook fires now.
    pub fn on_suspend(&self, hook: Box<dyn Fn() + Send>) {
        if self.is_suspended() {
            hook();
        }
        self.shared.on_suspend.lock().unwrap().push(hook);
    }

    /// Register a hook run when the task is resumed (`SIGCONT`).
    pub fn on_resume(&self, hook: Box<dyn Fn() + Send>) {
        self.shared.on_resume.lock().unwrap().push(hook);
    }

    pub fn same(&self, other: &TaskVal) -> bool {
        Arc::ptr_eq(&self.shared, &other.shared)
    }
}

// ---------------------------------------------------------------------------
// Closures
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct ClosureVal {
    /// `None` for lambdas; `Some` for `fn` declarations (drives `--help`).
    pub name: Option<String>,
    pub params: Vec<ast::Param>,
    pub rest: Option<ast::RestParam>,
    pub ret: Option<ast::Type>,
    pub body: ast::Expr,
    pub env: Env,
    pub doc: Option<String>,
}

#[derive(Clone)]
pub struct SecretVal {
    pub name: String,
    /// The secret material; never rendered, never journaled.
    pub value: Arc<str>,
}
impl std::fmt::Debug for SecretVal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("secret").field(&self.name).finish()
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// A shoal error value. Codes are pinned in docs/CONTRACTS.md §4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorVal {
    pub code: String,
    pub msg: String,
    pub span: Option<Span>,
    pub hint: Option<String>,
    pub stderr: Option<String>,
    /// External process exit status when this error originated from a command.
    pub status: Option<i32>,
}

impl ErrorVal {
    pub fn new(code: impl Into<String>, msg: impl Into<String>) -> ErrorVal {
        ErrorVal {
            code: code.into(),
            msg: msg.into(),
            span: None,
            hint: None,
            stderr: None,
            status: None,
        }
    }
    pub fn with_span(mut self, span: Span) -> ErrorVal {
        self.span = Some(span);
        self
    }
    /// Attach a span only if one isn't already present (innermost wins).
    pub fn or_span(mut self, span: Span) -> ErrorVal {
        self.span.get_or_insert(span);
        self
    }
    pub fn with_hint(mut self, hint: impl Into<String>) -> ErrorVal {
        self.hint = Some(hint.into());
        self
    }
    pub fn with_stderr(mut self, stderr: impl Into<String>) -> ErrorVal {
        self.stderr = Some(stderr.into());
        self
    }
    pub fn with_status(mut self, status: Option<i32>) -> ErrorVal {
        self.status = status;
        self
    }
    pub fn type_error(msg: impl Into<String>) -> ErrorVal {
        ErrorVal::new("type_error", msg)
    }
    pub fn arg_error(msg: impl Into<String>) -> ErrorVal {
        ErrorVal::new("arg_error", msg)
    }
}

impl std::fmt::Display for ErrorVal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.msg)
    }
}

impl std::error::Error for ErrorVal {}

// ---------------------------------------------------------------------------
// Environment (lexical scopes)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Env {
    inner: Arc<Mutex<EnvInner>>,
}

#[derive(Debug)]
struct EnvInner {
    vars: HashMap<String, Binding>,
    parent: Option<Env>,
}

#[derive(Debug, Clone)]
pub struct Binding {
    pub value: Value,
    pub mutable: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum AssignError {
    NotFound,
    Immutable,
}

impl Env {
    pub fn root() -> Env {
        Env {
            inner: Arc::new(Mutex::new(EnvInner {
                vars: HashMap::new(),
                parent: None,
            })),
        }
    }

    pub fn child(&self) -> Env {
        Env {
            inner: Arc::new(Mutex::new(EnvInner {
                vars: HashMap::new(),
                parent: Some(self.clone()),
            })),
        }
    }

    pub fn declare(&self, name: impl Into<String>, value: Value, mutable: bool) {
        self.inner
            .lock()
            .unwrap()
            .vars
            .insert(name.into(), Binding { value, mutable });
    }

    pub fn get(&self, name: &str) -> Option<Value> {
        let parent = {
            let g = self.inner.lock().unwrap();
            if let Some(b) = g.vars.get(name) {
                return Some(b.value.clone());
            }
            g.parent.clone()
        };
        parent.and_then(|p| p.get(name))
    }

    pub fn is_bound(&self, name: &str) -> bool {
        self.get(name).is_some()
    }

    /// Assign to an existing binding, walking up the scope chain.
    pub fn assign(&self, name: &str, value: Value) -> Result<(), AssignError> {
        let parent = {
            let mut g = self.inner.lock().unwrap();
            if let Some(b) = g.vars.get_mut(name) {
                if !b.mutable {
                    return Err(AssignError::Immutable);
                }
                b.value = value;
                return Ok(());
            }
            g.parent.clone()
        };
        match parent {
            Some(p) => p.assign(name, value),
            None => Err(AssignError::NotFound),
        }
    }

    /// Snapshot of every visible name (innermost shadowing wins) — for
    /// completion and introspection.
    pub fn visible_names(&self) -> Vec<String> {
        let mut names = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut cur = Some(self.clone());
        while let Some(env) = cur {
            let g = env.inner.lock().unwrap();
            for k in g.vars.keys() {
                if seen.insert(k.clone()) {
                    names.push(k.clone());
                }
            }
            cur = g.parent.clone();
        }
        names
    }
}

// ---------------------------------------------------------------------------
// Equality (structural for data, identity for stream/task)
// ---------------------------------------------------------------------------

impl PartialEq for Value {
    fn eq(&self, other: &Value) -> bool {
        use Value::*;
        match (self, other) {
            (Null, Null) => true,
            (Bool(a), Bool(b)) => a == b,
            (Int(a), Int(b)) => a == b,
            (Float(a), Float(b)) => a == b,
            // Mixed numeric equality compares numerically (promotion).
            (Int(a), Float(b)) | (Float(b), Int(a)) => (*a as f64) == *b,
            (Str(a), Str(b)) => a == b,
            (Path(a), Path(b)) => a == b,
            // A path and a str compare by display form (pragmatic equality).
            (Path(p), Str(s)) | (Str(s), Path(p)) => p.to_string_lossy() == *s,
            (Glob(a), Glob(b)) => a.pattern == b.pattern,
            (Regex(a), Regex(b)) => a.src == b.src,
            (Size(a), Size(b)) => a == b,
            (Duration(a), Duration(b)) => a == b,
            (DateTime(a), DateTime(b)) => a.timestamp() == b.timestamp(),
            (Time(a), Time(b)) => a == b,
            (Bytes(a), Bytes(b)) => a == b,
            (List(a), List(b)) => a == b,
            (Record(a), Record(b)) => a == b,
            (Table(a), Table(b)) => a == b,
            // A table is semantically a list<record>.
            (Table(t), List(l)) | (List(l), Table(t)) => {
                t.len() == l.len()
                    && t.iter().zip(l.iter()).all(|(r, v)| match v {
                        Record(vr) => r == vr,
                        _ => false,
                    })
            }
            (Range(a), Range(b)) => a == b,
            (Stream(a), Stream(b)) => a.same(b),
            (Error(a), Error(b)) => a == b,
            (Outcome(a), Outcome(b)) => Arc::ptr_eq(a, b),
            (Task(a), Task(b)) => a.same(b),
            (Closure(a), Closure(b)) => Arc::ptr_eq(a, b),
            (CmdRef(a), CmdRef(b)) => a == b,
            (Secret(a), Secret(b)) => a.name == b.name && a.value == b.value,
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Eval ↔ methods bridge (pinned in docs/CONTRACTS.md §7)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct CallArgs {
    pub pos: Vec<Value>,
    pub named: Vec<(String, Value)>,
}

impl CallArgs {
    pub fn get_named(&self, name: &str) -> Option<&Value> {
        self.named.iter().find(|(n, _)| n == name).map(|(_, v)| v)
    }
}

pub trait CallCtx {
    fn call_closure(&mut self, f: &Value, args: Vec<Value>) -> VResult<Value>;
    fn cwd(&self) -> PathBuf;
}

// ---------------------------------------------------------------------------
// Word parsing helpers (bind-time coercion, TDD §4.2 site 2)
// ---------------------------------------------------------------------------

/// Parse a size word like `1.5gb`, `4kib`, `237b`. Decimal units and binary
/// (`*ib`) units per TDD §2.1.
pub fn parse_size(word: &str) -> Option<u64> {
    let lower = word.to_ascii_lowercase();
    let split = lower.find(|c: char| c.is_ascii_alphabetic())?;
    let (num, unit) = lower.split_at(split);
    let num: f64 = num.parse().ok()?;
    let mult: f64 = match unit {
        "b" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1024.0 * 1024.0,
        "gib" => 1024.0 * 1024.0 * 1024.0,
        "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    if num < 0.0 {
        return None;
    }
    Some((num * mult).round() as u64)
}

/// Parse a duration word like `250ms`, `1.5h`, `30d`, or compound `1m30s`.
pub fn parse_duration(word: &str) -> Option<i64> {
    let lower = word.to_ascii_lowercase();
    let (neg, rest) = match lower.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, lower.as_str()),
    };
    let mut total: f64 = 0.0;
    let mut cur = rest;
    let mut any = false;
    while !cur.is_empty() {
        let split = cur.find(|c: char| c.is_ascii_alphabetic())?;
        if split == 0 {
            return None;
        }
        let (num, tail) = cur.split_at(split);
        let unit_end = tail
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(tail.len());
        let (unit, next) = tail.split_at(unit_end);
        let num: f64 = num.parse().ok()?;
        let ns: f64 = match unit {
            "ns" => 1.0,
            "us" => 1e3,
            "ms" => 1e6,
            "s" => 1e9,
            "m" => 60e9,
            "h" => 3_600e9,
            "d" => 86_400e9,
            "w" => 604_800e9,
            _ => return None,
        };
        total += num * ns;
        cur = next;
        any = true;
    }
    if !any {
        return None;
    }
    let v = total.round() as i64;
    Some(if neg { -v } else { v })
}

/// Parse a time word like `10:00am`, `23:15`, `07:30:15`.
pub fn parse_time(word: &str) -> Option<TimeVal> {
    let lower = word.to_ascii_lowercase();
    let (body, meridiem) = if let Some(b) = lower.strip_suffix("am") {
        (b, Some(false))
    } else if let Some(b) = lower.strip_suffix("pm") {
        (b, Some(true))
    } else {
        (lower.as_str(), None)
    };
    let parts: Vec<&str> = body.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    let mut hour: u8 = parts[0].parse().ok()?;
    let min: u8 = parts[1].parse().ok()?;
    let sec: u8 = if parts.len() == 3 {
        parts[2].parse().ok()?
    } else {
        0
    };
    match meridiem {
        Some(pm) => {
            if hour == 0 || hour > 12 {
                return None;
            }
            if pm && hour != 12 {
                hour += 12;
            }
            if !pm && hour == 12 {
                hour = 0;
            }
        }
        None => {
            if hour > 23 {
                return None;
            }
        }
    }
    if min > 59 || sec > 59 {
        return None;
    }
    Some(TimeVal { hour, min, sec })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_bytes_serialization() {
        // str → verbatim UTF-8, no trailing newline.
        assert_eq!(feed_bytes(&Value::Str("hi".into())).unwrap(), b"hi");
        // bytes → raw.
        assert_eq!(
            feed_bytes(&Value::Bytes(Arc::new(vec![1, 2, 3]))).unwrap(),
            vec![1, 2, 3]
        );
        // list<str> → newline-joined + trailing newline.
        assert_eq!(
            feed_bytes(&Value::List(vec![
                Value::Str("a".into()),
                Value::Str("b".into())
            ]))
            .unwrap(),
            b"a\nb\n"
        );
        // record → compact JSON, no trailing newline.
        let mut rec = Record::new();
        rec.insert("n".into(), Value::Int(2));
        assert_eq!(feed_bytes(&Value::Record(rec)).unwrap(), br#"{"n":2}"#);
        // table → row-major JSON.
        let mut row = Record::new();
        row.insert("x".into(), Value::Int(1));
        assert_eq!(
            feed_bytes(&Value::Table(vec![row])).unwrap(),
            br#"[{"x":1}]"#
        );
        // non-str list → JSON array.
        assert_eq!(
            feed_bytes(&Value::List(vec![Value::Int(3), Value::Int(1)])).unwrap(),
            b"[3,1]"
        );
    }

    #[test]
    fn feed_bytes_never_feedable_is_feed_error() {
        let e = feed_bytes(&Value::Secret(SecretVal {
            name: "tok".into(),
            value: Arc::from("x"),
        }))
        .unwrap_err();
        assert_eq!(e.code, "feed_error");
        assert!(e.hint.unwrap().contains("injected at spawn time"));
    }

    #[test]
    fn feed_bytes_unfeedable_scalar_is_type_error() {
        assert_eq!(feed_bytes(&Value::Int(4)).unwrap_err().code, "type_error");
    }

    #[test]
    fn env_scoping() {
        let root = Env::root();
        root.declare("x", Value::Int(1), false);
        let child = root.child();
        assert_eq!(child.get("x"), Some(Value::Int(1)));
        child.declare("x", Value::Int(2), true);
        assert_eq!(child.get("x"), Some(Value::Int(2)));
        assert_eq!(root.get("x"), Some(Value::Int(1)));
        assert_eq!(root.assign("x", Value::Int(9)), Err(AssignError::Immutable));
        assert_eq!(child.assign("x", Value::Int(3)), Ok(()));
        assert_eq!(child.get("x"), Some(Value::Int(3)));
    }

    #[test]
    fn stream_single_consumption() {
        let s = StreamVal::from_iter("int", (0..3).map(|i| Ok(Value::Int(i))));
        assert!(s.take_upstream().is_ok());
        let err = s.take_upstream().err().expect("second take must fail");
        assert_eq!(err.code, "stream_consumed");
    }

    #[test]
    fn parse_units() {
        assert_eq!(parse_size("1.5gb"), Some(1_500_000_000));
        assert_eq!(parse_size("4kib"), Some(4096));
        assert_eq!(parse_size("237b"), Some(237));
        assert_eq!(parse_size("nope"), None);
        assert_eq!(parse_duration("250ms"), Some(250_000_000));
        assert_eq!(parse_duration("1m30s"), Some(90_000_000_000));
        assert_eq!(parse_duration("1.5h"), Some(5_400_000_000_000));
        assert_eq!(
            parse_time("10:00am"),
            Some(TimeVal {
                hour: 10,
                min: 0,
                sec: 0
            })
        );
        assert_eq!(
            parse_time("12:15pm"),
            Some(TimeVal {
                hour: 12,
                min: 15,
                sec: 0
            })
        );
        assert_eq!(
            parse_time("12:15am"),
            Some(TimeVal {
                hour: 0,
                min: 15,
                sec: 0
            })
        );
        assert_eq!(
            parse_time("23:15"),
            Some(TimeVal {
                hour: 23,
                min: 15,
                sec: 0
            })
        );
    }

    #[test]
    fn json_uniform_objects_become_table() {
        let j: serde_json::Value = serde_json::from_str(r#"[{"a":1},{"a":2}]"#).unwrap();
        assert!(matches!(json_to_value(&j), Value::Table(_)));
    }
}
