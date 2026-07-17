//! The lazy stream combinator stages (site/content/internals/streams-channels.md), split out of
//! `stream/mod.rs` for size. Each wraps an inner [`Upstream`] and is
//! itself an [`Upstream`], so a chain composes by nesting.

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
    /// Seen values bucketed by a canonical hash (HR-G5, site/content/internals/streams-channels.md): membership
    /// is amortized O(1) instead of the old `Vec` linear scan (O(n²) total).
    /// Each bucket holds the (rare) hash collisions and is confirmed with
    /// `Value`'s real `PartialEq`, so cross-type equality (`1 == 1.0`, a
    /// `path` == its `str`, a `table` == the equal `list<record>`) is preserved
    /// exactly. Memory still grows with the number of DISTINCT values seen, so
    /// `distinct` on an unbounded stream of continuously-unique items grows
    /// without bound — bound it (`.take(n)`) or `dedupe` adjacent runs instead.
    pub seen: std::collections::HashMap<u64, Vec<Value>>,
}
impl Upstream for Distinct {
    fn pull(&mut self, ctx: &mut dyn CallCtx, t: Option<Duration>) -> VResult<Pull> {
        loop {
            match self.up.pull(ctx, t)? {
                Pull::Item(v) => {
                    let bucket = self.seen.entry(distinct_hash(&v)).or_default();
                    if bucket.iter().any(|seen| seen == &v) {
                        continue;
                    }
                    bucket.push(v.clone());
                    return Ok(Pull::Item(v));
                }
                other => return Ok(other),
            }
        }
    }
}

/// A canonical `u64` hash key for `distinct`'s membership set. The correctness
/// contract is `a == b` (`Value::PartialEq`) ⟹ `distinct_hash(a) ==
/// distinct_hash(b)`: equal values MUST share a bucket. A hash that is *coarser*
/// than equality only lengthens a bucket scan (buckets are re-checked with real
/// `PartialEq`); a hash that *splits* two equal values would wrongly emit both,
/// so every cross-type equality is canonicalized here. Pointer-identity and
/// rare-in-`distinct` types hash by a per-type tag only (correct — equal values
/// still collide — at the cost of a linear bucket for those element types).
fn distinct_hash(v: &Value) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    hash_value(v, &mut h);
    h.finish()
}

fn hash_value(v: &Value, h: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    match v {
        Value::Null => 0u8.hash(h),
        Value::Bool(b) => {
            1u8.hash(h);
            b.hash(h);
        }
        // Numeric cross-equality: `Int(n) == Float(n as f64)`, so both hash by
        // the same normalized `f64` bits (−0.0 → 0.0; NaN → a sentinel, since
        // NaN != NaN keeps every NaN distinct anyway).
        Value::Int(n) => {
            2u8.hash(h);
            norm_f64(*n as f64).hash(h);
        }
        Value::Float(f) => {
            2u8.hash(h);
            norm_f64(*f).hash(h);
        }
        // path/str cross-equality by display text.
        Value::Str(s) => {
            3u8.hash(h);
            s.as_bytes().hash(h);
        }
        Value::Path(p) => {
            3u8.hash(h);
            p.to_string_lossy().as_bytes().hash(h);
        }
        Value::Size(n) => {
            4u8.hash(h);
            n.hash(h);
        }
        Value::Duration(n) => {
            5u8.hash(h);
            n.hash(h);
        }
        // DateTime equality is instant equality (`a.timestamp() == b.timestamp()`),
        // so hash the instant, not the zoned wall-clock rendering.
        Value::DateTime(d) => {
            6u8.hash(h);
            d.timestamp().as_nanosecond().hash(h);
        }
        Value::Bytes(b) => {
            8u8.hash(h);
            b.as_slice().hash(h);
        }
        Value::CasBytes(c) => {
            9u8.hash(h);
            c.hash.as_bytes().hash(h);
            c.len.hash(h);
        }
        // table ≡ list<record>: both fold the SAME per-record hash under one
        // container tag, so an equal table and list<record> collide.
        Value::List(xs) => {
            10u8.hash(h);
            for x in xs {
                hash_value(x, h);
            }
        }
        Value::Table(rows) => {
            10u8.hash(h);
            for r in rows {
                hash_record(r, h);
            }
        }
        Value::Record(r) => hash_record(r, h),
        Value::Range(r) => {
            12u8.hash(h);
            r.start.hash(h);
            r.end.hash(h);
            r.inclusive.hash(h);
        }
        Value::Glob(g) => {
            13u8.hash(h);
            g.pattern.as_bytes().hash(h);
        }
        Value::Regex(re) => {
            14u8.hash(h);
            re.src.as_bytes().hash(h);
        }
        // Pointer-identity / rare-in-`distinct` types: a per-type tag is enough
        // for the collide-then-verify contract (equal values still share the
        // bucket). These element types degrade `distinct` to a linear bucket.
        Value::Time(_) => 15u8.hash(h),
        Value::Error(_) => 16u8.hash(h),
        Value::Outcome(_) => 17u8.hash(h),
        Value::Task(_) => 18u8.hash(h),
        Value::Closure(_) => 19u8.hash(h),
        Value::CmdRef(_) => 20u8.hash(h),
        Value::Secret(_) => 21u8.hash(h),
        Value::Stream(_) => 22u8.hash(h),
    }
}

fn hash_record(r: &super::Record, h: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    11u8.hash(h);
    for (k, val) in r {
        k.as_bytes().hash(h);
        hash_value(val, h);
    }
}

/// Normalize an `f64` so equal numeric values hash identically: `+0.0`/`−0.0`
/// collapse to one key, and every NaN maps to one sentinel (NaN != NaN keeps
/// them all distinct at the `PartialEq` verify step anyway).
fn norm_f64(x: f64) -> u64 {
    if x == 0.0 {
        0
    } else if x.is_nan() {
        u64::MAX
    } else {
        x.to_bits()
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
