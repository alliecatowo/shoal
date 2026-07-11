//! Value-method standard library. Methods are deliberately pure except the
//! explicit filesystem sinks (`save` and `append`).

use crate::{CallArgs, CallCtx, ErrorVal, Record, StreamVal, VResult, Value, value_to_json};
use shoal_ast::Span;
use std::cmp::Ordering;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

pub fn call_method(
    ctx: &mut dyn CallCtx,
    recv: Value,
    name: &str,
    args: CallArgs,
    span: Span,
) -> VResult<Value> {
    dispatch(ctx, recv, name, args).map_err(|e| e.or_span(span))
}

fn dispatch(ctx: &mut dyn CallCtx, recv: Value, name: &str, args: CallArgs) -> VResult<Value> {
    // Outcome unification (P1b): an unknown method on a command outcome forwards
    // to its structured `.out`, so `ls.where(.size > 1b).sort(.name)` works
    // (`ls` is an outcome; `.where`/`.sort` operate on its `.out` table). Raw
    // stream bytes stay reachable via `.stdout`/`.stderr`.
    if let Value::Outcome(o) = &recv {
        match name {
            "stdout" => return Ok(Value::Bytes(o.stdout.clone())),
            "stderr" => return Ok(Value::Bytes(o.stderr.clone())),
            _ => {
                let inner = o.out_value();
                return dispatch(ctx, inner, name, args);
            }
        }
    }
    match name {
        // `.feed(cmd)` (IO.md §1) spawns a child, which a pure value method
        // cannot do — the evaluator intercepts `.feed` in its method-call path
        // (shoal-eval `expr.rs::eval_feed`) before `call_method` is ever
        // reached, building an `ExecSpec` with the value's `feed_bytes` as
        // stdin. Reaching here means `.feed` was called through a path that
        // bypassed that bridge; surface a clear error rather than "no method".
        "feed" => Err(ErrorVal::type_error(
            ".feed must be evaluated by the interpreter, not as a pure value method",
        )),
        "len" | "count" => no_args(&args).and_then(|_| len(recv)),
        "is_empty" => no_args(&args)
            .and_then(|_| len(recv))
            .map(|v| Value::Bool(v == Value::Int(0))),
        "first" => first_last(recv, &args, true),
        "last" => first_last(recv, &args, false),
        "collect" => collect(recv),
        "tee" => tee(recv, int_arg(&args, 0, 2)?),
        "map" => map(ctx, recv, arg(&args, 0)?),
        "where" | "filter" => filter(ctx, recv, arg(&args, 0)?),
        "each" => each(ctx, recv, arg(&args, 0)?),
        "any" => any_all(ctx, recv, arg(&args, 0)?, true),
        "all" => any_all(ctx, recv, arg(&args, 0)?, false),
        "find" => find(ctx, recv, arg(&args, 0)?),
        "flat_map" => flat_map(ctx, recv, arg(&args, 0)?),
        "sort_by" => sort_by(ctx, recv, arg(&args, 0)?),
        // `sort(.key)` sorts by the key extractor (e.g. `ls.sort(.name)`);
        // `sort()` with no argument sorts the elements directly.
        "sort" => {
            if args.pos.is_empty() {
                sort(recv)
            } else {
                sort_by(ctx, recv, arg(&args, 0)?)
            }
        }
        "reverse" => reverse(recv),
        "uniq" => uniq(recv),
        "sum" => sum(recv),
        "min" => minmax(recv, false),
        "max" => minmax(recv, true),
        "flatten" => flatten(recv),
        "enumerate" => enumerate(recv),
        "skip" => slice_count(recv, int_arg(&args, 0, 0)?, false),
        "take" => slice_count(recv, int_arg(&args, 0, 0)?, true),
        "chunks" => chunks(recv, int_arg(&args, 0, 0)?),
        "zip" => zip(recv, arg(&args, 0)?.clone()),
        "group" => group(ctx, recv, arg(&args, 0)?),
        "join" => join(recv, str_arg(&args, 0, "")?),
        "lines" => string_unary(recv, |s| {
            Value::List(
                s.lines()
                    .map(|x| Value::Str(x.trim_end_matches('\r').into()))
                    .collect(),
            )
        }),
        "words" => string_unary(recv, |s| {
            Value::List(s.split_whitespace().map(|x| Value::Str(x.into())).collect())
        }),
        "chars" => string_unary(recv, |s| {
            Value::List(s.chars().map(|x| Value::Str(x.to_string())).collect())
        }),
        "trim" => string_unary(recv, |s| Value::Str(s.trim().into())),
        "upper" => string_unary(recv, |s| Value::Str(s.to_uppercase())),
        "lower" => string_unary(recv, |s| Value::Str(s.to_lowercase())),
        "split" => {
            let sep = str_arg(&args, 0, "")?;
            string_unary(recv, |s| {
                Value::List(s.split(sep).map(|x| Value::Str(x.into())).collect())
            })
        }
        "starts_with" => string_pred(recv, str_arg(&args, 0, "")?, |s, q| s.starts_with(q)),
        "ends_with" => string_pred(recv, str_arg(&args, 0, "")?, |s, q| s.ends_with(q)),
        "contains" => contains(recv, arg(&args, 0)?),
        "replace" => {
            let a = str_arg(&args, 0, "")?;
            let b = str_arg(&args, 1, "")?;
            string_unary(recv, |s| Value::Str(s.replace(a, b)))
        }
        "matches" => matches_method(recv, arg(&args, 0)?),
        "match" => match_method(recv, arg(&args, 0)?),
        "parse_int" => string_parse(recv, "int"),
        "parse_float" => string_parse(recv, "float"),
        "keys" => record_side(recv, true),
        "values" => record_side(recv, false),
        "items" => items(recv),
        "get" => get(
            recv,
            arg(&args, 0)?,
            args.pos.get(1).cloned().unwrap_or(Value::Null),
        ),
        "str" => to_str(recv, false),
        "display" => to_str(recv, true),
        "json" => Ok(Value::Str(
            serde_json::to_string(&value_to_json(&recv))
                .map_err(|e| ErrorVal::new("custom", e.to_string()))?,
        )),
        "abs" => numeric_unary(recv, f64::abs, i64::checked_abs),
        "round" => float_unary(recv, f64::round),
        "floor" => float_unary(recv, f64::floor),
        "ceil" => float_unary(recv, f64::ceil),
        "save" => save(ctx, recv, arg(&args, 0)?, false),
        "append" => save(ctx, recv, arg(&args, 0)?, true),
        // Task lifecycle methods (defect #14).
        "await" | "wait" => no_args(&args).and_then(|_| task_await(recv)),
        "cancel" => no_args(&args).and_then(|_| task_cancel(recv)),
        "is_done" => no_args(&args).and_then(|_| task_is_done(recv)),
        _ => Err(ErrorVal::new(
            "field_missing",
            format!("unknown method `.{name}` on {}", recv.type_name()),
        )),
    }
}

fn arg(args: &CallArgs, n: usize) -> VResult<&Value> {
    args.pos
        .get(n)
        .ok_or_else(|| ErrorVal::arg_error(format!("missing argument {}", n + 1)))
}
fn no_args(args: &CallArgs) -> VResult<()> {
    if args.pos.is_empty() && args.named.is_empty() {
        Ok(())
    } else {
        Err(ErrorVal::arg_error("method takes no arguments"))
    }
}
fn int_arg(args: &CallArgs, n: usize, default: i64) -> VResult<usize> {
    match args.pos.get(n) {
        None => Ok(default.max(0) as usize),
        Some(Value::Int(i)) if *i >= 0 => Ok(*i as usize),
        Some(v) => Err(ErrorVal::type_error(format!(
            "expected non-negative int, found {}",
            v.type_name()
        ))),
    }
}
fn str_arg<'a>(args: &'a CallArgs, n: usize, default: &'a str) -> VResult<&'a str> {
    match args.pos.get(n) {
        None => Ok(default),
        Some(Value::Str(s)) => Ok(s),
        Some(v) => Err(ErrorVal::type_error(format!(
            "expected str, found {}",
            v.type_name()
        ))),
    }
}
fn seq(v: Value) -> VResult<Vec<Value>> {
    match v {
        Value::List(x) => Ok(x),
        Value::Table(x) => Ok(x.into_iter().map(Value::Record).collect()),
        Value::Range(r) => Ok(r.iter().map(Value::Int).collect()),
        Value::Stream(s) => s.take()?.collect(),
        v => Err(ErrorVal::type_error(format!(
            "expected collection, found {}",
            v.type_name()
        ))),
    }
}
fn len(v: Value) -> VResult<Value> {
    Ok(Value::Int(match v {
        Value::Str(s) => s.chars().count(),
        Value::Bytes(b) => b.len(),
        Value::List(x) => x.len(),
        Value::Table(x) => x.len(),
        Value::Record(x) => x.len(),
        Value::Range(r) => r.len(),
        Value::Stream(s) => s.take()?.collect::<VResult<Vec<_>>>()?.len(),
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
fn first_last(v: Value, args: &CallArgs, first: bool) -> VResult<Value> {
    if args.pos.is_empty() && args.named.is_empty() {
        let mut x = seq(v)?;
        return Ok(if first {
            x.into_iter().next().unwrap_or(Value::Null)
        } else {
            x.pop().unwrap_or(Value::Null)
        });
    }
    let n = int_arg(args, 0, 0)?;
    let x = seq(v)?;
    let out: Vec<Value> = if first {
        x.into_iter().take(n).collect()
    } else {
        let skip = x.len().saturating_sub(n);
        x.into_iter().skip(skip).collect()
    };
    Ok(Value::List(out))
}
fn collect(v: Value) -> VResult<Value> {
    match v {
        Value::Stream(s) => Ok(Value::List(s.take()?.collect::<VResult<_>>()?)),
        Value::Range(r) => Ok(Value::List(r.iter().map(Value::Int).collect())),
        x @ Value::List(_) | x @ Value::Table(_) => Ok(x),
        v => Err(ErrorVal::type_error(format!(
            "cannot collect {}",
            v.type_name()
        ))),
    }
}
fn tee(v: Value, n: usize) -> VResult<Value> {
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
fn map(ctx: &mut dyn CallCtx, v: Value, f: &Value) -> VResult<Value> {
    Ok(Value::List(
        seq(v)?
            .into_iter()
            .map(|x| ctx.call_closure(f, vec![x]))
            .collect::<VResult<_>>()?,
    ))
}
fn filter(ctx: &mut dyn CallCtx, v: Value, f: &Value) -> VResult<Value> {
    let mut out = vec![];
    for x in seq(v)? {
        if ctx.call_closure(f, vec![x.clone()])?.as_condition()? {
            out.push(x)
        }
    }
    Ok(Value::List(out))
}
fn each(ctx: &mut dyn CallCtx, v: Value, f: &Value) -> VResult<Value> {
    for x in seq(v)? {
        ctx.call_closure(f, vec![x])?;
    }
    Ok(Value::Null)
}
fn any_all(ctx: &mut dyn CallCtx, v: Value, f: &Value, any: bool) -> VResult<Value> {
    for x in seq(v)? {
        let b = ctx.call_closure(f, vec![x])?.as_condition()?;
        if b == any {
            return Ok(Value::Bool(any));
        }
    }
    Ok(Value::Bool(!any))
}
fn find(ctx: &mut dyn CallCtx, v: Value, f: &Value) -> VResult<Value> {
    for x in seq(v)? {
        if ctx.call_closure(f, vec![x.clone()])?.as_condition()? {
            return Ok(x);
        }
    }
    Ok(Value::Null)
}
fn flat_map(ctx: &mut dyn CallCtx, v: Value, f: &Value) -> VResult<Value> {
    let mut o = vec![];
    for x in seq(v)? {
        o.extend(seq(ctx.call_closure(f, vec![x])?)?)
    }
    Ok(Value::List(o))
}
fn sort_by(ctx: &mut dyn CallCtx, v: Value, f: &Value) -> VResult<Value> {
    let mut keyed = vec![];
    for x in seq(v)? {
        keyed.push((ctx.call_closure(f, vec![x.clone()])?, x));
    }
    keyed.sort_by(|a, b| cmp(&a.0, &b.0).unwrap_or(Ordering::Equal));
    Ok(Value::List(keyed.into_iter().map(|x| x.1).collect()))
}
fn sort(v: Value) -> VResult<Value> {
    let mut x = seq(v)?;
    for pair in x.windows(2) {
        cmp(&pair[0], &pair[1])?;
    }
    x.sort_by(|a, b| cmp(a, b).unwrap_or(Ordering::Equal));
    Ok(Value::List(x))
}
fn reverse(v: Value) -> VResult<Value> {
    match v {
        Value::Str(s) => Ok(Value::Str(s.chars().rev().collect())),
        v => {
            let mut x = seq(v)?;
            x.reverse();
            Ok(Value::List(x))
        }
    }
}
fn uniq(v: Value) -> VResult<Value> {
    let mut out = vec![];
    for x in seq(v)? {
        if !out.contains(&x) {
            out.push(x)
        }
    }
    Ok(Value::List(out))
}
fn sum(v: Value) -> VResult<Value> {
    let mut a = Value::Int(0);
    for x in seq(v)? {
        a = crate::ops::binop(shoal_ast::BinOp::Add, &a, &x)?;
    }
    Ok(a)
}
fn minmax(v: Value, max: bool) -> VResult<Value> {
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
fn cmp(a: &Value, b: &Value) -> VResult<Ordering> {
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
fn flatten(v: Value) -> VResult<Value> {
    let mut o = vec![];
    for x in seq(v)? {
        o.extend(seq(x)?)
    }
    Ok(Value::List(o))
}
fn enumerate(v: Value) -> VResult<Value> {
    Ok(Value::List(
        seq(v)?
            .into_iter()
            .enumerate()
            .map(|(i, x)| Value::List(vec![Value::Int(i as i64), x]))
            .collect(),
    ))
}
fn slice_count(v: Value, n: usize, take: bool) -> VResult<Value> {
    let x = seq(v)?;
    Ok(Value::List(if take {
        x.into_iter().take(n).collect()
    } else {
        x.into_iter().skip(n).collect()
    }))
}
fn chunks(v: Value, n: usize) -> VResult<Value> {
    if n == 0 {
        return Err(ErrorVal::arg_error("chunk size must be positive"));
    }
    Ok(Value::List(
        seq(v)?.chunks(n).map(|x| Value::List(x.to_vec())).collect(),
    ))
}
fn zip(a: Value, b: Value) -> VResult<Value> {
    Ok(Value::List(
        seq(a)?
            .into_iter()
            .zip(seq(b)?)
            .map(|(a, b)| Value::List(vec![a, b]))
            .collect(),
    ))
}
fn group(ctx: &mut dyn CallCtx, v: Value, f: &Value) -> VResult<Value> {
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
fn join(v: Value, sep: &str) -> VResult<Value> {
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
fn string_unary(v: Value, f: impl FnOnce(&str) -> Value) -> VResult<Value> {
    match v {
        Value::Str(s) => Ok(f(&s)),
        v => Err(ErrorVal::type_error(format!(
            "expected str, found {}",
            v.type_name()
        ))),
    }
}
fn string_pred(v: Value, q: &str, f: fn(&str, &str) -> bool) -> VResult<Value> {
    string_unary(v, |s| Value::Bool(f(s, q)))
}
fn contains(v: Value, q: &Value) -> VResult<Value> {
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
fn matches_method(v: Value, q: &Value) -> VResult<Value> {
    match (v, q) {
        (Value::Str(s), Value::Regex(r)) => Ok(Value::List(
            r.re.find_iter(&s)
                .map(|m| Value::Str(m.as_str().into()))
                .collect(),
        )),
        _ => Err(ErrorVal::type_error(
            "matches expects str receiver and regex",
        )),
    }
}
fn match_method(v: Value, q: &Value) -> VResult<Value> {
    match (v, q) {
        (Value::Str(s), Value::Regex(r)) => Ok(r
            .re
            .find(&s)
            .map(|m| Value::Str(m.as_str().into()))
            .unwrap_or(Value::Null)),
        _ => Err(ErrorVal::type_error("match expects str receiver and regex")),
    }
}
fn string_parse(v: Value, ty: &str) -> VResult<Value> {
    match v {
        Value::Str(s) => match ty {
            "int" => s
                .parse()
                .map(Value::Int)
                .map_err(|_| ErrorVal::arg_error(format!("cannot parse {s:?} as int"))),
            _ => s
                .parse()
                .map(Value::Float)
                .map_err(|_| ErrorVal::arg_error(format!("cannot parse {s:?} as float"))),
        },
        v => Err(ErrorVal::type_error(format!(
            "expected str, found {}",
            v.type_name()
        ))),
    }
}
fn record_side(v: Value, keys: bool) -> VResult<Value> {
    match v {
        Value::Record(r) => Ok(Value::List(if keys {
            r.keys().cloned().map(Value::Str).collect()
        } else {
            r.into_values().collect()
        })),
        v => Err(ErrorVal::type_error(format!(
            "expected record, found {}",
            v.type_name()
        ))),
    }
}
fn items(v: Value) -> VResult<Value> {
    match v {
        Value::Record(r) => Ok(Value::List(
            r.into_iter()
                .map(|(k, v)| Value::List(vec![Value::Str(k), v]))
                .collect(),
        )),
        v => Err(ErrorVal::type_error(format!(
            "expected record, found {}",
            v.type_name()
        ))),
    }
}
fn get(v: Value, key: &Value, default: Value) -> VResult<Value> {
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
fn to_str(v: Value, lossy: bool) -> VResult<Value> {
    match v {
        Value::Str(s) => Ok(Value::Str(s)),
        Value::Path(p) => {
            if lossy {
                Ok(Value::Str(p.to_string_lossy().into()))
            } else {
                p.into_os_string()
                    .into_string()
                    .map(Value::Str)
                    .map_err(|_| ErrorVal::new("utf8_error", "path is not valid UTF-8"))
            }
        }
        Value::Bytes(b) => {
            if lossy {
                Ok(Value::Str(String::from_utf8_lossy(&b).into()))
            } else {
                String::from_utf8((*b).clone())
                    .map(Value::Str)
                    .map_err(|_| ErrorVal::new("utf8_error", "bytes are not valid UTF-8"))
            }
        }
        v => Err(ErrorVal::type_error(format!(
            "cannot convert {} to str",
            v.type_name()
        ))),
    }
}
fn numeric_unary(v: Value, ff: fn(f64) -> f64, fi: fn(i64) -> Option<i64>) -> VResult<Value> {
    match v {
        Value::Int(i) => fi(i)
            .map(Value::Int)
            .ok_or_else(|| ErrorVal::new("custom", "integer overflow")),
        Value::Float(f) => Ok(Value::Float(ff(f))),
        v => Err(ErrorVal::type_error(format!(
            "expected number, found {}",
            v.type_name()
        ))),
    }
}
fn float_unary(v: Value, f: fn(f64) -> f64) -> VResult<Value> {
    match v {
        Value::Float(x) => Ok(Value::Float(f(x))),
        Value::Int(i) => Ok(Value::Int(i)),
        v => Err(ErrorVal::type_error(format!(
            "expected number, found {}",
            v.type_name()
        ))),
    }
}
fn task_await(recv: Value) -> VResult<Value> {
    match recv {
        Value::Task(t) => t.wait(),
        v => Err(ErrorVal::type_error(format!(
            ".await expects a task, found {}",
            v.type_name()
        ))),
    }
}
fn task_cancel(recv: Value) -> VResult<Value> {
    match recv {
        Value::Task(t) => {
            t.cancel();
            Ok(Value::Null)
        }
        v => Err(ErrorVal::type_error(format!(
            ".cancel expects a task, found {}",
            v.type_name()
        ))),
    }
}
fn task_is_done(recv: Value) -> VResult<Value> {
    match recv {
        Value::Task(t) => Ok(Value::Bool(t.is_done())),
        v => Err(ErrorVal::type_error(format!(
            ".is_done expects a task, found {}",
            v.type_name()
        ))),
    }
}
fn save(ctx: &mut dyn CallCtx, v: Value, path: &Value, append: bool) -> VResult<Value> {
    let p = match path {
        Value::Path(p) => p.clone(),
        Value::Str(s) => PathBuf::from(s),
        v => {
            return Err(ErrorVal::type_error(format!(
                "expected path, found {}",
                v.type_name()
            )));
        }
    };
    let p = if p.is_absolute() {
        p
    } else {
        ctx.cwd().join(p)
    };
    let bytes = match &v {
        Value::Bytes(b) => (**b).clone(),
        Value::Str(s) => s.as_bytes().to_vec(),
        _ => serde_json::to_vec(&value_to_json(&v))
            .map_err(|e| ErrorVal::new("custom", e.to_string()))?,
    };
    let mut o = OpenOptions::new();
    o.create(true).write(true);
    if append {
        o.append(true)
    } else {
        o.truncate(true)
    };
    o.open(&p)
        .and_then(|mut f| f.write_all(&bytes))
        .map_err(|e| ErrorVal::new("custom", format!("{}: {e}", p.display())))?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RegexVal, StreamVal};
    struct C {
        cwd: PathBuf,
    }
    impl CallCtx for C {
        fn call_closure(&mut self, f: &Value, args: Vec<Value>) -> VResult<Value> {
            match f {
                Value::Str(s) if s == "double" => match args[0] {
                    Value::Int(i) => Ok(Value::Int(i * 2)),
                    _ => unreachable!(),
                },
                Value::Str(s) if s == "even" => match args[0] {
                    Value::Int(i) => Ok(Value::Bool(i % 2 == 0)),
                    _ => unreachable!(),
                },
                _ => Err(ErrorVal::new("custom", "bad test callback")),
            }
        }
        fn cwd(&self) -> PathBuf {
            self.cwd.clone()
        }
    }
    fn c() -> C {
        C {
            cwd: std::env::temp_dir(),
        }
    }
    fn a(xs: Vec<Value>) -> CallArgs {
        CallArgs {
            pos: xs,
            named: vec![],
        }
    }
    fn call(v: Value, n: &str, args: Vec<Value>) -> VResult<Value> {
        call_method(&mut c(), v, n, a(args), Span::default())
    }
    #[test]
    fn collection_basics() {
        let x = Value::List(vec![Value::Int(3), Value::Int(1), Value::Int(3)]);
        assert_eq!(
            call(x.clone(), "sort", vec![]).unwrap(),
            Value::List(vec![Value::Int(1), Value::Int(3), Value::Int(3)])
        );
        assert_eq!(
            call(x, "uniq", vec![]).unwrap(),
            Value::List(vec![Value::Int(3), Value::Int(1)])
        );
    }
    #[test]
    fn higher_order() {
        let x = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        assert_eq!(
            call(x.clone(), "map", vec![Value::Str("double".into())]).unwrap(),
            Value::List(vec![Value::Int(2), Value::Int(4), Value::Int(6)])
        );
        assert_eq!(
            call(x, "where", vec![Value::Str("even".into())]).unwrap(),
            Value::List(vec![Value::Int(2)])
        );
    }
    #[test]
    fn stream_consumption_and_tee() {
        let s = StreamVal::from_iter("int", (0..3).map(|i| Ok(Value::Int(i))));
        let clone = s.clone();
        assert!(
            matches!(call(Value::Stream(s),"collect",vec![]).unwrap(),Value::List(x) if x.len()==3)
        );
        assert_eq!(
            call(Value::Stream(clone), "collect", vec![])
                .unwrap_err()
                .code,
            "stream_consumed"
        );
        let t = StreamVal::from_iter("int", (0..2).map(|i| Ok(Value::Int(i))));
        assert!(
            matches!(call(Value::Stream(t),"tee",vec![Value::Int(2)]).unwrap(),Value::List(x) if x.len()==2)
        );
    }
    #[test]
    fn strings_regex_records() {
        assert_eq!(
            call(Value::Str(" a b ".into()), "trim", vec![]).unwrap(),
            Value::Str("a b".into())
        );
        let re = Value::Regex(std::sync::Arc::new(RegexVal::compile("[0-9]+").unwrap()));
        assert_eq!(
            call(Value::Str("a12b3".into()), "matches", vec![re]).unwrap(),
            Value::List(vec![Value::Str("12".into()), Value::Str("3".into())])
        );
        let mut r = Record::new();
        r.insert("a".into(), Value::Int(1));
        assert_eq!(
            call(Value::Record(r), "get", vec![Value::Str("a".into())]).unwrap(),
            Value::Int(1)
        );
    }
    #[test]
    fn chunks_flatten_sum() {
        let x = Value::List((1..=5).map(Value::Int).collect());
        assert!(
            matches!(call(x.clone(),"chunks",vec![Value::Int(2)]).unwrap(),Value::List(v) if v.len()==3)
        );
        assert_eq!(call(x, "sum", vec![]).unwrap(), Value::Int(15));
        let nested = Value::List(vec![
            Value::List(vec![Value::Int(1)]),
            Value::List(vec![Value::Int(2)]),
        ]);
        assert_eq!(
            call(nested, "flatten", vec![]).unwrap(),
            Value::List(vec![Value::Int(1), Value::Int(2)])
        );
    }
    #[test]
    fn first_last_arity_variants() {
        let x = Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
        // Zero-arg forms return a single element.
        assert_eq!(call(x.clone(), "first", vec![]).unwrap(), Value::Int(1));
        assert_eq!(call(x.clone(), "last", vec![]).unwrap(), Value::Int(3));
        // `.first(n)`/`.last(n)` return a LIST of n (P3 arity fix).
        assert_eq!(
            call(x.clone(), "first", vec![Value::Int(2)]).unwrap(),
            Value::List(vec![Value::Int(1), Value::Int(2)])
        );
        assert_eq!(
            call(x.clone(), "last", vec![Value::Int(2)]).unwrap(),
            Value::List(vec![Value::Int(2), Value::Int(3)])
        );
        // Overrun clamps to the collection length (no error).
        assert_eq!(
            call(x, "first", vec![Value::Int(9)]).unwrap(),
            Value::List(vec![Value::Int(1), Value::Int(2), Value::Int(3)])
        );
    }

    #[test]
    fn outcome_methods_forward_to_out() {
        use crate::OutcomeVal;
        use std::sync::Arc;
        // An outcome whose `.out` is a list forwards collection methods.
        let outcome = Value::Outcome(Arc::new(OutcomeVal {
            status: Some(0),
            signal: None,
            ok: true,
            stdout: Arc::new(Vec::new()),
            stderr: Arc::new(Vec::new()),
            dur_ns: 0,
            pid: 0,
            cmd: "x".into(),
            parsed: Some(Value::List(vec![
                Value::Int(1),
                Value::Int(2),
                Value::Int(3),
            ])),
            streamed: false,
        }));
        assert_eq!(call(outcome.clone(), "len", vec![]).unwrap(), Value::Int(3));
        assert_eq!(
            call(outcome, "first", vec![Value::Int(2)]).unwrap(),
            Value::List(vec![Value::Int(1), Value::Int(2)])
        );
    }

    #[test]
    fn task_lifecycle_methods() {
        let t = crate::TaskVal::new("t");
        t.finish(Ok(Value::Int(42)));
        assert_eq!(
            call(Value::Task(t.clone()), "is_done", vec![]).unwrap(),
            Value::Bool(true)
        );
        assert_eq!(
            call(Value::Task(t.clone()), "await", vec![]).unwrap(),
            Value::Int(42)
        );
        assert_eq!(call(Value::Task(t), "cancel", vec![]).unwrap(), Value::Null);
        // Wrong receiver type is a type error.
        assert_eq!(
            call(Value::Int(1), "await", vec![]).unwrap_err().code,
            "type_error"
        );
    }

    #[test]
    fn save_and_append() {
        let d = std::env::temp_dir().join(format!("shoal-methods-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let mut ctx = C { cwd: d.clone() };
        call_method(
            &mut ctx,
            Value::Str("a".into()),
            "save",
            a(vec![Value::Str("x".into())]),
            Span::default(),
        )
        .unwrap();
        call_method(
            &mut ctx,
            Value::Str("b".into()),
            "append",
            a(vec![Value::Str("x".into())]),
            Span::default(),
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(d.join("x")).unwrap(), "ab");
        std::fs::remove_dir_all(d).unwrap();
    }
}
