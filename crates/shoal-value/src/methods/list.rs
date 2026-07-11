//! Collection ops shared by `list`/`table`/`range` (via [`seq`]), plus a
//! couple of receiver-polymorphic ones (`.contains`, `.get`) that don't
//! cleanly belong to any single receiver type.

use super::*;
use std::cmp::Ordering;

pub(crate) fn seq(v: Value) -> VResult<Vec<Value>> {
    match v {
        Value::List(x) => Ok(x),
        Value::Table(x) => Ok(x.into_iter().map(Value::Record).collect()),
        Value::Range(r) => Ok(r.iter().map(Value::Int).collect()),
        // Streams are intercepted at the top of `dispatch` and driven with `ctx`;
        // a stream nested inside another collection is materialized by the
        // stream sink path, not here — reaching this arm means a raw stream was
        // handed to a pure, ctx-less op, which cannot drive it.
        Value::Stream(_) => Err(ErrorVal::new(
            "stream_consumed",
            "a stream must be collected with `.collect()` before this operation",
        )),
        v => Err(ErrorVal::type_error(format!(
            "expected collection, found {}",
            v.type_name()
        ))),
    }
}

pub(crate) fn cmp(a: &Value, b: &Value) -> VResult<Ordering> {
    match (a, b) {
        (Value::Int(a), Value::Int(b)) => Ok(a.cmp(b)),
        (Value::Float(a), Value::Float(b)) => a
            .partial_cmp(b)
            .ok_or_else(|| ErrorVal::type_error("NaN is not orderable")),
        (Value::Int(a), Value::Float(b)) => (*a as f64)
            .partial_cmp(b)
            .ok_or_else(|| ErrorVal::type_error("NaN is not orderable")),
        (Value::Float(a), Value::Int(b)) => a
            .partial_cmp(&(*b as f64))
            .ok_or_else(|| ErrorVal::type_error("NaN is not orderable")),
        (Value::Str(a), Value::Str(b)) => Ok(a.cmp(b)),
        (Value::Path(a), Value::Path(b)) => Ok(a.cmp(b)),
        _ => Err(ErrorVal::type_error(format!(
            "cannot compare {} and {}",
            a.type_name(),
            b.type_name()
        ))),
    }
}

pub(crate) fn len(v: Value) -> VResult<Value> {
    Ok(Value::Int(match v {
        Value::Str(s) => s.chars().count(),
        Value::Bytes(b) => b.len(),
        Value::List(x) => x.len(),
        Value::Table(x) => x.len(),
        Value::Record(x) => x.len(),
        Value::Range(r) => r.len(),
        v => {
            return Err(ErrorVal::type_error(format!(
                ".len unsupported on {}",
                v.type_name()
            )));
        }
    } as i64))
}

/// `.first()`/`.last()` return a single element; `.first(n)`/`.last(n)` return
/// a LIST of the first/last `n` (P3 arity fix — `.first(2)` was wrongly
/// yielding a single record, breaking `…​.first(2).map(.name)`).
pub(crate) fn first_last(v: Value, args: &CallArgs, first: bool) -> VResult<Value> {
    if args.pos.is_empty() && args.named.is_empty() {
        let mut x = seq(v)?;
        return Ok(if first {
            x.into_iter().next().unwrap_or(Value::Null)
        } else {
            x.pop().unwrap_or(Value::Null)
        });
    }
    let n = super::int_arg(args, 0, 0)?;
    let x = seq(v)?;
    let out: Vec<Value> = if first {
        x.into_iter().take(n).collect()
    } else {
        let skip = x.len().saturating_sub(n);
        x.into_iter().skip(skip).collect()
    };
    Ok(Value::List(out))
}

pub(crate) fn collect(v: Value) -> VResult<Value> {
    match v {
        Value::Range(r) => Ok(Value::List(r.iter().map(Value::Int).collect())),
        x @ Value::List(_) | x @ Value::Table(_) => Ok(x),
        v => Err(ErrorVal::type_error(format!(
            "cannot collect {}",
            v.type_name()
        ))),
    }
}

/// `.stream()` — promote a finite value into a `stream<T>`. Collections stream
/// their elements; a string streams its lines; bytes stream their UTF-8 lines.
pub(crate) fn to_stream(v: Value) -> VResult<Value> {
    let items: Vec<Value> = match v {
        Value::List(x) => x,
        Value::Table(x) => x.into_iter().map(Value::Record).collect(),
        Value::Range(r) => r.iter().map(Value::Int).collect(),
        Value::Str(s) => s.lines().map(|l| Value::Str(l.to_string())).collect(),
        Value::Bytes(b) => String::from_utf8_lossy(&b)
            .lines()
            .map(|l| Value::Str(l.to_string()))
            .collect(),
        v => {
            return Err(ErrorVal::type_error(format!(
                "cannot make a stream from {}",
                v.type_name()
            )));
        }
    };
    Ok(Value::Stream(StreamVal::from_iter(
        "value",
        items.into_iter().map(Ok),
    )))
}

pub(crate) fn tee(v: Value, n: usize) -> VResult<Value> {
    if n == 0 {
        return Err(ErrorVal::arg_error("tee count must be positive"));
    }
    let x = seq(v)?;
    Ok(Value::List(
        (0..n)
            .map(|_| Value::Stream(StreamVal::from_iter("value", x.clone().into_iter().map(Ok))))
            .collect(),
    ))
}
pub(crate) fn map(ctx: &mut dyn CallCtx, v: Value, f: &Value) -> VResult<Value> {
    Ok(Value::List(
        seq(v)?
            .into_iter()
            .map(|x| ctx.call_closure(f, vec![x]))
            .collect::<VResult<_>>()?,
    ))
}
pub(crate) fn filter(ctx: &mut dyn CallCtx, v: Value, f: &Value) -> VResult<Value> {
    let mut out = vec![];
    for x in seq(v)? {
        if ctx.call_closure(f, vec![x.clone()])?.as_condition()? {
            out.push(x)
        }
    }
    Ok(Value::List(out))
}
pub(crate) fn each(ctx: &mut dyn CallCtx, v: Value, f: &Value) -> VResult<Value> {
    for x in seq(v)? {
        ctx.call_closure(f, vec![x])?;
    }
    Ok(Value::Null)
}
pub(crate) fn any_all(ctx: &mut dyn CallCtx, v: Value, f: &Value, any: bool) -> VResult<Value> {
    for x in seq(v)? {
        let b = ctx.call_closure(f, vec![x])?.as_condition()?;
        if b == any {
            return Ok(Value::Bool(any));
        }
    }
    Ok(Value::Bool(!any))
}
pub(crate) fn find(ctx: &mut dyn CallCtx, v: Value, f: &Value) -> VResult<Value> {
    for x in seq(v)? {
        if ctx.call_closure(f, vec![x.clone()])?.as_condition()? {
            return Ok(x);
        }
    }
    Ok(Value::Null)
}
pub(crate) fn flat_map(ctx: &mut dyn CallCtx, v: Value, f: &Value) -> VResult<Value> {
    let mut o = vec![];
    for x in seq(v)? {
        o.extend(seq(ctx.call_closure(f, vec![x])?)?)
    }
    Ok(Value::List(o))
}
pub(crate) fn sort_by(ctx: &mut dyn CallCtx, v: Value, f: &Value) -> VResult<Value> {
    let mut keyed = vec![];
    for x in seq(v)? {
        keyed.push((ctx.call_closure(f, vec![x.clone()])?, x));
    }
    keyed.sort_by(|a, b| cmp(&a.0, &b.0).unwrap_or(Ordering::Equal));
    Ok(Value::List(keyed.into_iter().map(|x| x.1).collect()))
}
pub(crate) fn sort(v: Value) -> VResult<Value> {
    let mut x = seq(v)?;
    for pair in x.windows(2) {
        cmp(&pair[0], &pair[1])?;
    }
    x.sort_by(|a, b| cmp(a, b).unwrap_or(Ordering::Equal));
    Ok(Value::List(x))
}
pub(crate) fn reverse(v: Value) -> VResult<Value> {
    match v {
        Value::Str(s) => Ok(Value::Str(s.chars().rev().collect())),
        v => {
            let mut x = seq(v)?;
            x.reverse();
            Ok(Value::List(x))
        }
    }
}
pub(crate) fn uniq(v: Value) -> VResult<Value> {
    let mut out = vec![];
    for x in seq(v)? {
        if !out.contains(&x) {
            out.push(x)
        }
    }
    Ok(Value::List(out))
}
pub(crate) fn sum(v: Value) -> VResult<Value> {
    let mut a = Value::Int(0);
    for x in seq(v)? {
        a = crate::ops::binop(shoal_ast::BinOp::Add, &a, &x)?;
    }
    Ok(a)
}
pub(crate) fn minmax(v: Value, max: bool) -> VResult<Value> {
    let mut it = seq(v)?.into_iter();
    let Some(mut best) = it.next() else {
        return Ok(Value::Null);
    };
    for x in it {
        let o = cmp(&x, &best)?;
        if (max && o == Ordering::Greater) || (!max && o == Ordering::Less) {
            best = x
        }
    }
    Ok(best)
}
pub(crate) fn flatten(v: Value) -> VResult<Value> {
    let mut o = vec![];
    for x in seq(v)? {
        o.extend(seq(x)?)
    }
    Ok(Value::List(o))
}
pub(crate) fn enumerate(v: Value) -> VResult<Value> {
    Ok(Value::List(
        seq(v)?
            .into_iter()
            .enumerate()
            .map(|(i, x)| Value::List(vec![Value::Int(i as i64), x]))
            .collect(),
    ))
}
pub(crate) fn slice_count(v: Value, n: usize, take: bool) -> VResult<Value> {
    let x = seq(v)?;
    Ok(Value::List(if take {
        x.into_iter().take(n).collect()
    } else {
        x.into_iter().skip(n).collect()
    }))
}
pub(crate) fn chunks(v: Value, n: usize) -> VResult<Value> {
    if n == 0 {
        return Err(ErrorVal::arg_error("chunk size must be positive"));
    }
    Ok(Value::List(
        seq(v)?.chunks(n).map(|x| Value::List(x.to_vec())).collect(),
    ))
}
pub(crate) fn zip(a: Value, b: Value) -> VResult<Value> {
    Ok(Value::List(
        seq(a)?
            .into_iter()
            .zip(seq(b)?)
            .map(|(a, b)| Value::List(vec![a, b]))
            .collect(),
    ))
}
pub(crate) fn group(ctx: &mut dyn CallCtx, v: Value, f: &Value) -> VResult<Value> {
    let mut groups: Vec<(Value, Vec<Value>)> = vec![];
    for x in seq(v)? {
        let k = ctx.call_closure(f, vec![x.clone()])?;
        if let Some((_, g)) = groups.iter_mut().find(|(a, _)| a == &k) {
            g.push(x)
        } else {
            groups.push((k, vec![x]))
        }
    }
    Ok(Value::List(
        groups
            .into_iter()
            .map(|(k, v)| {
                let mut r = Record::new();
                r.insert("key".into(), k);
                r.insert("values".into(), Value::List(v));
                Value::Record(r)
            })
            .collect(),
    ))
}
pub(crate) fn join(v: Value, sep: &str) -> VResult<Value> {
    let mut out = vec![];
    for x in seq(v)? {
        match x {
            Value::Str(s) => out.push(s),
            v => {
                return Err(ErrorVal::type_error(format!(
                    "join expects str elements, found {}",
                    v.type_name()
                )));
            }
        }
    }
    Ok(Value::Str(out.join(sep)))
}
pub(crate) fn contains(v: Value, q: &Value) -> VResult<Value> {
    match v {
        Value::Str(s) => match q {
            Value::Str(q) => Ok(Value::Bool(s.contains(q))),
            _ => Err(ErrorVal::type_error("string contains expects str")),
        },
        Value::Record(r) => match q {
            Value::Str(k) => Ok(Value::Bool(r.contains_key(k))),
            _ => Err(ErrorVal::type_error("record contains expects str key")),
        },
        v => Ok(Value::Bool(seq(v)?.contains(q))),
    }
}
pub(crate) fn get(v: Value, key: &Value, default: Value) -> VResult<Value> {
    match (v, key) {
        (Value::Record(r), Value::Str(k)) => Ok(r.get(k).cloned().unwrap_or(default)),
        (Value::List(x), Value::Int(i)) => Ok(if *i >= 0 {
            x.get(*i as usize)
        } else {
            x.get(x.len().wrapping_sub((-i) as usize))
        }
        .cloned()
        .unwrap_or(default)),
        _ => Err(ErrorVal::type_error("get expects record+str or list+int")),
    }
}
