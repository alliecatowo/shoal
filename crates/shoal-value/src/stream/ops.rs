//! The lazy stream combinator stages (STREAMS §3), split out of
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
