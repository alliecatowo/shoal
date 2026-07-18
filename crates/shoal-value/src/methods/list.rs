//! Collection ops shared by `list`/`table`/`range` (via [`seq`]), plus a
//! couple of receiver-polymorphic ones (`.contains`, `.get`) that don't
//! cleanly belong to any single receiver type.

use super::*;
use std::cmp::Ordering;

pub(crate) fn seq(v: Value) -> VResult<Vec<Value>> {
    match v {
        Value::List(x) => Ok(x),
        Value::Table(x) => Ok(x.into_iter().map(Value::Record).collect()),
        Value::Range(r) => r.materialize(),
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

/// Ordering used by `.sort()`/`.min()`/`.max()`. Delegates to the same
/// total-order comparator as the `<`/`>`/`<=`/`>=` operators
/// ([`crate::ops::compare`]) so every directly-comparable value type
/// (int/float/str/path/size/duration/datetime/time/bool) sorts consistently
/// with how it compares — no narrower, separately-maintained arm set.
pub(crate) fn cmp(a: &Value, b: &Value) -> VResult<Ordering> {
    crate::ops::compare(a, b)
}

pub(crate) fn len(v: Value) -> VResult<Value> {
    if let Value::Range(range) = v {
        return i64::try_from(range.len()).map(Value::Int).map_err(|_| {
            ErrorVal::new(
                "range_length_overflow",
                "range length exceeds the language integer limit",
            )
        });
    }
    let len = match v {
        Value::Str(s) => s.chars().count(),
        Value::Bytes(b) => b.len(),
        Value::List(x) => x.len(),
        Value::Table(x) => x.len(),
        Value::Record(x) => x.len(),
        v => {
            return Err(ErrorVal::type_error(format!(
                ".len unsupported on {}",
                v.type_name()
            )));
        }
    };
    i64::try_from(len)
        .map(Value::Int)
        .map_err(|_| ErrorVal::new("collection_length_overflow", "collection is too large"))
}

/// `.first()`/`.last()` return a single element; `.first(n)`/`.last(n)` return
/// a LIST of the first/last `n` (`.first(2)` once wrongly
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
        Value::Range(r) => r.materialize().map(Value::List),
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
        Value::Range(r) => {
            return Ok(Value::Stream(StreamVal::from_iter(
                "int",
                r.iter().map(|value| Ok(Value::Int(value))),
            )));
        }
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
    if n == 0 || n > StreamVal::TEE_MAX_FORKS {
        return Err(ErrorVal::arg_error(format!(
            "tee count must be between 1 and {}",
            StreamVal::TEE_MAX_FORKS
        )));
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
/// `.reduce(init, f)` — left fold: thread `acc` through `f(acc, x)` for each
/// element, starting from `init`, returning the final accumulator. This is the
/// collection-only terminal counterpart to stream `.scan`.
/// general escape hatch when no named aggregation (`.sum`/`.min`/…) fits.
pub(crate) fn reduce(ctx: &mut dyn CallCtx, v: Value, init: Value, f: &Value) -> VResult<Value> {
    let mut acc = init;
    for x in seq(v)? {
        acc = ctx.call_closure(f, vec![acc, x])?;
    }
    Ok(acc)
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
    // Pre-validate the keys and PROPAGATE the comparison error, exactly like
    // `sort()` below (and unlike a bare `unwrap_or(Equal)`, which silently
    // mis-orders heterogeneous/null/NaN keys). `[{k:1},{k:"a"},{k:2}].sort(.k)`
    // must error consistently with `[1,"a",2].sort()`.
    for pair in keyed.windows(2) {
        cmp(&pair[0].0, &pair[1].0)?;
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
    // Accumulate from the first element (not Int(0)) so a homogeneous list of a
    // non-int numeric type sums in that type: list<size> -> size, list<duration>
    // -> duration, list<float> -> float. An empty list is still Int(0).
    let mut it = seq(v)?.into_iter();
    let Some(mut a) = it.next() else {
        return Ok(Value::Int(0));
    };
    for x in it {
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
    // `.take`/`.skip` also slice a `str` — by char, returning a substring — so
    // text parsing reads naturally: `line.take(7)` is the git short hash,
    // `line.skip(8)` is the message. (List/table/range keep their semantics.)
    if let Value::Str(s) = &v {
        let sliced: String = if take {
            s.chars().take(n).collect()
        } else {
            s.chars().skip(n).collect()
        };
        return Ok(Value::Str(sliced));
    }
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
        (Value::List(x), Value::Int(i)) => Ok(index(*i, x.len())
            .and_then(|i| x.get(i).cloned())
            .unwrap_or(default)),
        (Value::Table(x), Value::Int(i)) => Ok(index(*i, x.len())
            .and_then(|i| x.get(i).cloned())
            .map(Value::Record)
            .unwrap_or(default)),
        (Value::Range(r), Value::Int(i)) => Ok(r.value_at(*i).map(Value::Int).unwrap_or(default)),
        _ => Err(ErrorVal::type_error(
            "get expects record+str or list/table/range+int",
        )),
    }
}

fn index(index: i64, len: usize) -> Option<usize> {
    if index >= 0 {
        usize::try_from(index).ok().filter(|index| *index < len)
    } else {
        usize::try_from(index.unsigned_abs())
            .ok()
            .and_then(|distance| len.checked_sub(distance))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_indexes_tables_and_ranges_without_materializing() {
        let mut row = Record::new();
        row.insert("name".into(), Value::Str("shoal".into()));
        assert_eq!(
            get(Value::Table(vec![row.clone()]), &Value::Int(0), Value::Null).unwrap(),
            Value::Record(row)
        );
        let range = || {
            Value::Range(RangeVal {
                start: 10,
                end: 13,
                inclusive: false,
            })
        };
        assert_eq!(
            get(range(), &Value::Int(1), Value::Null).unwrap(),
            Value::Int(11)
        );
        assert_eq!(
            get(range(), &Value::Int(-1), Value::Null).unwrap(),
            Value::Int(12)
        );
        assert_eq!(
            get(range(), &Value::Int(8), Value::Int(99)).unwrap(),
            Value::Int(99)
        );
        assert_eq!(
            get(
                Value::List(vec![Value::Int(1)]),
                &Value::Int(i64::MIN),
                Value::Int(99),
            )
            .unwrap(),
            Value::Int(99)
        );
    }
}
