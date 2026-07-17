//! The lazy stream combinator stages (site/content/internals/streams-channels.md), split out of
//! `stream/mod.rs` for size. Each wraps an inner [`Upstream`] and is
//! itself an [`Upstream`], so a chain composes by nesting.

use super::{CallCtx, Pull, Upstream, VResult, Value};
use std::collections::{HashMap, VecDeque, hash_map::DefaultHasher};
use std::hash::{Hash, Hasher};
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

/// Sequential concat-map. Each expansion is exhausted before the next outer
/// item is requested; this stage does not claim concurrent/interleaved child
/// stream semantics.
pub struct FlatMapSequential {
    pub up: Box<dyn Upstream>,
    pub f: Value,
    pub sub: Option<Box<dyn Upstream>>,
    pub queue: VecDeque<Value>,
}
impl Upstream for FlatMapSequential {
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
    /// Equality-compatible semantic hash to collision bucket. The final check
    /// always uses `Value::eq`, so hashing is an accelerator rather than a new
    /// definition of language equality. Like every exact `distinct`, this
    /// retains one clone per unique value until the stream ends.
    pub seen: HashMap<u64, Vec<Value>>,
}
impl Upstream for Distinct {
    fn pull(&mut self, ctx: &mut dyn CallCtx, t: Option<Duration>) -> VResult<Pull> {
        loop {
            match self.up.pull(ctx, t)? {
                Pull::Item(v) => {
                    let bucket = self.seen.entry(semantic_hash(&v)).or_default();
                    if bucket.contains(&v) {
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

/// Hash exactly the equality semantics implemented by `Value::eq` where a
/// useful structural hash exists. Identity values use their identity pointer.
/// Collisions are always safe because the bucket performs the final equality
/// check.
fn semantic_hash(value: &Value) -> u64 {
    fn record_hash<H: Hasher>(record: &super::Record, state: &mut H) {
        // IndexMap equality is key/value equality, independent of insertion
        // order, so canonicalize keys before hashing.
        let mut fields: Vec<_> = record.iter().collect();
        fields.sort_unstable_by_key(|(key, _)| *key);
        fields.len().hash(state);
        for (key, value) in fields {
            key.hash(state);
            value_hash(value, state);
        }
    }

    fn sequence_hash<'a, H: Hasher>(
        values: impl ExactSizeIterator<Item = &'a Value>,
        state: &mut H,
    ) {
        13_u8.hash(state);
        values.len().hash(state);
        for value in values {
            value_hash(value, state);
        }
    }

    fn value_hash<H: Hasher>(value: &Value, state: &mut H) {
        match value {
            Value::Null => 0_u8.hash(state),
            Value::Bool(v) => {
                1_u8.hash(state);
                v.hash(state);
            }
            // Mixed int/float equality promotes the integer to f64. Hash both
            // through that same representation, normalizing signed zero.
            Value::Int(v) => {
                2_u8.hash(state);
                (*v as f64).to_bits().hash(state);
            }
            Value::Float(v) => {
                2_u8.hash(state);
                (if *v == 0.0 { 0 } else { v.to_bits() }).hash(state);
            }
            // Path/str cross equality compares the path's display form.
            Value::Str(v) => {
                3_u8.hash(state);
                v.hash(state);
            }
            Value::Path(v) => {
                3_u8.hash(state);
                v.to_string_lossy().hash(state);
            }
            Value::Glob(v) => {
                4_u8.hash(state);
                // Value equality intentionally considers only the pattern.
                v.pattern.hash(state);
            }
            Value::Regex(v) => {
                5_u8.hash(state);
                v.src.hash(state);
            }
            Value::Size(v) => {
                6_u8.hash(state);
                v.hash(state);
            }
            Value::Duration(v) => {
                7_u8.hash(state);
                v.hash(state);
            }
            Value::DateTime(v) => {
                8_u8.hash(state);
                v.timestamp().as_nanosecond().hash(state);
            }
            Value::Time(v) => {
                9_u8.hash(state);
                (v.hour, v.min, v.sec).hash(state);
            }
            Value::Bytes(v) => {
                10_u8.hash(state);
                v.hash(state);
            }
            Value::CasBytes(v) => {
                11_u8.hash(state);
                v.hash.hash(state);
                v.len.hash(state);
            }
            Value::List(values) => sequence_hash(values.iter(), state),
            Value::Table(rows) => {
                // A table equals a list<record>, so use the list tag and the
                // same record representation used by Value::Record below.
                13_u8.hash(state);
                rows.len().hash(state);
                for row in rows {
                    14_u8.hash(state);
                    record_hash(row, state);
                }
            }
            Value::Record(record) => {
                14_u8.hash(state);
                record_hash(record, state);
            }
            Value::Range(v) => {
                15_u8.hash(state);
                (v.start, v.end, v.inclusive).hash(state);
            }
            Value::Stream(v) => {
                16_u8.hash(state);
                std::sync::Arc::as_ptr(&v.inner).hash(state);
            }
            Value::Error(v) => {
                17_u8.hash(state);
                v.code.hash(state);
                v.msg.hash(state);
                v.span.map(|s| (s.start, s.end)).hash(state);
                v.hint.hash(state);
                v.stderr.hash(state);
                v.status.hash(state);
            }
            Value::Outcome(v) => {
                18_u8.hash(state);
                std::sync::Arc::as_ptr(v).hash(state);
            }
            Value::Task(v) => {
                19_u8.hash(state);
                std::sync::Arc::as_ptr(&v.shared).hash(state);
            }
            Value::Closure(v) => {
                20_u8.hash(state);
                std::sync::Arc::as_ptr(v).hash(state);
            }
            Value::CmdRef(v) => {
                21_u8.hash(state);
                // Equal AST nodes serialize identically. If serialization ever
                // fails, the tag remains a conservative correct fallback.
                if let Ok(bytes) = serde_json::to_vec(v.as_ref()) {
                    bytes.hash(state);
                }
            }
            Value::Secret(v) => {
                22_u8.hash(state);
                v.name.hash(state);
                v.value.hash(state);
            }
        }
    }

    let mut state = DefaultHasher::new();
    value_hash(value, &mut state);
    state.finish()
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
    /// Which ready input gets first refusal. Flipped after every emitted item
    /// so an always-ready source cannot starve its peer.
    pub prefer_a: bool,
}
impl Upstream for Merge {
    fn pull(&mut self, ctx: &mut dyn CallCtx, t: Option<Duration>) -> VResult<Pull> {
        let deadline = t.map(|d| Instant::now() + d);
        loop {
            if self.a_done && self.b_done {
                return Ok(Pull::End);
            }

            // Probe both sides without blocking, preferred side first. This is
            // strict round-robin when both are ready and lets a fast side run
            // freely when its peer has no item (merge has no skew queue).
            for poll_a in [self.prefer_a, !self.prefer_a] {
                if poll_a && !self.a_done {
                    match self.a.pull(ctx, Some(Duration::ZERO))? {
                        Pull::Item(v) => {
                            self.prefer_a = false;
                            return Ok(Pull::Item(v));
                        }
                        Pull::End => self.a_done = true,
                        Pull::Timeout => {}
                    }
                } else if !poll_a && !self.b_done {
                    match self.b.pull(ctx, Some(Duration::ZERO))? {
                        Pull::Item(v) => {
                            self.prefer_a = true;
                            return Ok(Pull::Item(v));
                        }
                        Pull::End => self.b_done = true,
                        Pull::Timeout => {}
                    }
                }
            }

            if self.a_done && self.b_done {
                return Ok(Pull::End);
            }
            if deadline.is_some_and(|dl| Instant::now() >= dl) {
                return Ok(Pull::Timeout);
            }

            let step = match deadline {
                Some(dl) => dl.saturating_duration_since(Instant::now()).min(POLL),
                None => POLL,
            };
            let wait_a = if self.a_done {
                false
            } else if self.b_done {
                true
            } else {
                self.prefer_a
            };
            if wait_a {
                match self.a.pull(ctx, Some(step))? {
                    Pull::Item(v) => {
                        self.prefer_a = false;
                        return Ok(Pull::Item(v));
                    }
                    Pull::End => self.a_done = true,
                    Pull::Timeout => self.prefer_a = false,
                }
            } else {
                match self.b.pull(ctx, Some(step))? {
                    Pull::Item(v) => {
                        self.prefer_a = true;
                        return Ok(Pull::Item(v));
                    }
                    Pull::End => self.b_done = true,
                    Pull::Timeout => self.prefer_a = true,
                }
            }
        }
    }
}

pub struct Zip {
    pub a: Box<dyn Upstream>,
    pub b: Box<dyn Upstream>,
    /// At most one unpaired item per side. A fast side is backpressured here
    /// until its peer supplies the matching positional item.
    pub pending_a: Option<Value>,
    pub pending_b: Option<Value>,
    pub wait_a: bool,
    pub done: bool,
}
impl Upstream for Zip {
    fn pull(&mut self, ctx: &mut dyn CallCtx, t: Option<Duration>) -> VResult<Pull> {
        if self.done {
            return Ok(Pull::End);
        }
        let deadline = t.map(|d| Instant::now() + d);
        loop {
            // Non-blocking probes make progress on either side. Crucially, an
            // item remains pending across Timeout instead of being discarded.
            if self.pending_a.is_none() {
                match self.a.pull(ctx, Some(Duration::ZERO))? {
                    Pull::Item(v) => self.pending_a = Some(v),
                    Pull::End => {
                        self.done = true;
                        self.pending_b = None;
                        return Ok(Pull::End);
                    }
                    Pull::Timeout => {}
                }
            }
            if self.pending_b.is_none() {
                match self.b.pull(ctx, Some(Duration::ZERO))? {
                    Pull::Item(v) => self.pending_b = Some(v),
                    Pull::End => {
                        self.done = true;
                        self.pending_a = None;
                        return Ok(Pull::End);
                    }
                    Pull::Timeout => {}
                }
            }
            if self.pending_a.is_some() && self.pending_b.is_some() {
                let a = self.pending_a.take().expect("checked pending a");
                let b = self.pending_b.take().expect("checked pending b");
                return Ok(Pull::Item(Value::List(vec![a, b])));
            }
            if deadline.is_some_and(|dl| Instant::now() >= dl) {
                return Ok(Pull::Timeout);
            }

            let step = match deadline {
                Some(dl) => dl.saturating_duration_since(Instant::now()).min(POLL),
                None => POLL,
            };
            let missing_a = self.pending_a.is_none();
            let missing_b = self.pending_b.is_none();
            let poll_a = missing_a && (!missing_b || self.wait_a);
            if poll_a {
                match self.a.pull(ctx, Some(step))? {
                    Pull::Item(v) => self.pending_a = Some(v),
                    Pull::End => {
                        self.done = true;
                        self.pending_b = None;
                        return Ok(Pull::End);
                    }
                    Pull::Timeout => self.wait_a = false,
                }
            } else {
                match self.b.pull(ctx, Some(step))? {
                    Pull::Item(v) => self.pending_b = Some(v),
                    Pull::End => {
                        self.done = true;
                        self.pending_a = None;
                        return Ok(Pull::End);
                    }
                    Pull::Timeout => self.wait_a = true,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Fs, OutcomeVal, Record, StdFs};
    use shoal_ast::Span;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    struct Ctx;
    impl CallCtx for Ctx {
        fn call_closure(&mut self, _f: &Value, _args: Vec<Value>) -> VResult<Value> {
            Err(super::super::ErrorVal::new("custom", "unexpected closure"))
        }

        fn cwd(&self) -> PathBuf {
            PathBuf::from("/")
        }

        fn fs(&self) -> &dyn Fs {
            static FS: StdFs = StdFs;
            &FS
        }
    }

    enum Step {
        Item(i64),
        Timeout,
        End,
    }

    struct Script(VecDeque<Step>);
    impl Upstream for Script {
        fn pull(&mut self, _ctx: &mut dyn CallCtx, _t: Option<Duration>) -> VResult<Pull> {
            Ok(match self.0.pop_front().unwrap_or(Step::End) {
                Step::Item(v) => Pull::Item(Value::Int(v)),
                Step::Timeout => Pull::Timeout,
                Step::End => Pull::End,
            })
        }
    }

    fn script(steps: impl IntoIterator<Item = Step>) -> Box<dyn Upstream> {
        Box::new(Script(steps.into_iter().collect()))
    }

    #[test]
    fn merge_round_robins_two_always_ready_inputs() {
        let mut merge = Merge {
            a: script([Step::Item(1), Step::Item(2), Step::End]),
            b: script([Step::Item(10), Step::Item(20), Step::End]),
            a_done: false,
            b_done: false,
            prefer_a: true,
        };
        let mut got = Vec::new();
        loop {
            match merge.pull(&mut Ctx, None).unwrap() {
                Pull::Item(Value::Int(v)) => got.push(v),
                Pull::Item(v) => panic!("unexpected value: {v:?}"),
                Pull::End => break,
                Pull::Timeout => panic!("an un-timed pull must not time out"),
            }
        }
        assert_eq!(got, [1, 10, 2, 20]);
    }

    #[test]
    fn zip_retains_the_fast_item_across_timeout() {
        let mut zip = Zip {
            a: script([Step::Item(1), Step::End]),
            b: script([Step::Timeout, Step::Item(10), Step::End]),
            pending_a: None,
            pending_b: None,
            wait_a: true,
            done: false,
        };
        assert!(matches!(
            zip.pull(&mut Ctx, Some(Duration::ZERO)).unwrap(),
            Pull::Timeout
        ));
        assert_eq!(zip.pending_a, Some(Value::Int(1)));
        assert!(matches!(
            zip.pull(&mut Ctx, Some(Duration::ZERO)).unwrap(),
            Pull::Item(Value::List(pair)) if pair == [Value::Int(1), Value::Int(10)]
        ));
    }

    #[test]
    fn zip_rate_skew_is_bounded_to_one_pending_item() {
        let mut zip = Zip {
            a: script([Step::Item(1), Step::Item(2), Step::End]),
            b: script([Step::Timeout, Step::Timeout, Step::Item(10), Step::End]),
            pending_a: None,
            pending_b: None,
            wait_a: true,
            done: false,
        };
        for _ in 0..2 {
            assert!(matches!(
                zip.pull(&mut Ctx, Some(Duration::ZERO)).unwrap(),
                Pull::Timeout
            ));
            assert_eq!(zip.pending_a, Some(Value::Int(1)));
        }
        assert!(matches!(
            zip.pull(&mut Ctx, Some(Duration::ZERO)).unwrap(),
            Pull::Item(Value::List(pair)) if pair == [Value::Int(1), Value::Int(10)]
        ));
    }

    fn assert_equal_hash(a: Value, b: Value) {
        assert_eq!(a, b, "test pair must exercise Value equality");
        assert_eq!(semantic_hash(&a), semantic_hash(&b));
    }

    #[test]
    fn semantic_hash_matches_cross_variant_and_nested_value_equality() {
        assert_equal_hash(Value::Int(2), Value::Float(2.0));
        assert_equal_hash(Value::Float(-0.0), Value::Float(0.0));
        assert_equal_hash(
            Value::Path(Path::new("a/b").into()),
            Value::Str("a/b".into()),
        );
        assert_equal_hash(Value::Null, Value::Null);

        let mut left = Record::new();
        left.insert("a".into(), Value::Int(1));
        left.insert("b".into(), Value::List(vec![Value::Int(2)]));
        let mut right = Record::new();
        right.insert("b".into(), Value::List(vec![Value::Float(2.0)]));
        right.insert("a".into(), Value::Float(1.0));
        assert_equal_hash(Value::Record(left.clone()), Value::Record(right.clone()));
        assert_equal_hash(
            Value::Table(vec![left]),
            Value::List(vec![Value::Record(right)]),
        );

        let error = Arc::new(
            super::super::ErrorVal::new("x", "boom")
                .with_span(Span::new(2, 4))
                .with_hint("h")
                .with_stderr("stderr")
                .with_status(Some(7)),
        );
        assert_equal_hash(Value::Error(error.clone()), Value::Error(error));

        let outcome = Arc::new(OutcomeVal {
            status: Some(0),
            signal: None,
            ok: true,
            stdout: Arc::new(Vec::new()),
            stdout_ref: None,
            stderr: Arc::new(Vec::new()),
            dur_ns: 0,
            pid: 1,
            cmd: "x".into(),
            parsed: None,
            streamed: false,
            span: None,
        });
        assert_equal_hash(Value::Outcome(outcome.clone()), Value::Outcome(outcome));
    }
}
