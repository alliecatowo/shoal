//! Value-method standard library. Methods are deliberately pure except the
//! explicit filesystem sinks (`save` and `append`).
//!
//! # Module layout
//!
//! This file holds `call_method`'s dispatch table (grouped by receiver type)
//! plus the small argument-decoding helpers every arm shares. The actual
//! method bodies live one per receiver-type module:
//!
//! - [`list`] — collection ops shared by `list`/`table`/`range` (via `seq`),
//!   plus a couple of receiver-polymorphic ones (`.contains`, `.get`).
//! - [`strops`] — string ops.
//! - [`record`] — record-only ops (`.keys`/`.values`/`.items`).
//! - [`path`] — `.save`/`.append`.
//! - [`num`] — numeric unary ops.
//! - [`stream`] — the `stream<T>` method surface (site/content/internals/streams-channels.md).
//! - [`outcome`] — outcome method forwarding (P1b unification).
//! - [`task`] — task lifecycle methods (site/content/internals/language-conformance-contract.md job control).
//! - [`suggest`] — did-you-mean hints for the unknown-method fall-through.

mod list;
mod num;
mod outcome;
mod path;
mod record;
mod stream;
mod strops;
mod suggest;
mod task;

pub use suggest::{levenshtein, method_names, methods_for};

use super::*;

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
    // `.tap(f)` / `.also(f)` — shoal's answer to bash `tee`: run `f(recv)` for
    // its side effect (save it, log it, inspect it) and return `recv`
    // UNCHANGED so the dot-chain keeps flowing. `cmd | tee file | next`
    // becomes `cmd.tap(o => o.stdout.save(file)).<next>`. Intercepted before
    // the outcome/stream forwarding so it taps the ACTUAL receiver (e.g. a
    // command's full outcome, not its `.out`). `f`'s own result is discarded.
    if matches!(name, "tap" | "also") {
        let f = arg(&args, 0)?;
        ctx.call_closure(f, vec![recv.clone()])?;
        return Ok(recv);
    }
    // Lazy CAS-backed bytes (see `site/content/internals/persistence.md`). The
    // cheap, no-load answers (`len`/`count`/`is_empty`/`ref`) are the ONE
    // `CasBytesVal::cheap_method` chokepoint — every site that needs a
    // metadata-only answer calls it instead of hand-rolling this match, so it
    // can't silently drift out of sync with `json_preview`'s equivalent
    // metadata-only answer for a NESTED occurrence (`crate::json`).
    // `.load`/`.bytes` materialize to a resident `bytes`; anything else
    // materializes the full content once and re-dispatches through the normal
    // `bytes` path, so no per-method arm has to know about CAS backing. This
    // is the one call site where a full CAS load of a bare (non-nested)
    // CasBytes value is deliberate and expected — see `crate::json`'s doc
    // comment on why the nested case (a CasBytes buried in a record/table
    // field) does NOT take this path.
    if let Value::CasBytes(c) = &recv {
        if let Some(v) = c.cheap_method(name) {
            return Ok(v);
        }
        match name {
            "load" | "bytes" => {
                return c.resolve().map(|b| Value::Bytes(std::sync::Arc::new(b)));
            }
            _ => {
                let full = c.resolve()?;
                return dispatch(ctx, Value::Bytes(std::sync::Arc::new(full)), name, args);
            }
        }
    }
    // Outcome unification: an unknown method on a command outcome forwards
    // to its structured `.out`, so `ls.where(.size > 1b).sort(.name)` works
    // (`ls` is an outcome; `.where`/`.sort` operate on its `.out` table). Raw
    // stream bytes stay reachable via `.stdout`/`.stderr`.
    if let Value::Outcome(o) = &recv {
        return outcome::forward(ctx, o, name, args);
    }
    // Streams (site/content/internals/streams-channels.md) get their own method surface: the lazy
    // combinators (site/content/internals/language-conformance-contract.md) return a NEW stream without driving the source, and the
    // sinks (site/content/internals/language-conformance-contract.md) drive it (with `ctx` for closure stages). Anything else falls
    // through to the collection methods by materializing a *bounded* stream to a
    // list first (an unbounded stream errors `stream_unbounded`).
    if let Value::Stream(s) = recv {
        return stream::stream_method(ctx, s, name, args);
    }
    // Pure (no-IO) `path` component accessors (site/content/internals/intercrate-protocol-contracts.md). Intercepted
    // ahead of the generic table because `.abs` on a path means "absolutize",
    // not the numeric `.abs`. The filesystem-backed path methods (`.read`,
    // `.lines`, `.size`, …) are handled earlier still, in the evaluator, since
    // they need the `Fs` port.
    if let Value::Path(p) = &recv {
        match name {
            "name" | "stem" | "ext" => return path::component(p, name),
            "parent" => return no_args(&args).map(|_| path::parent(p)),
            "join" => return path::join(p, arg(&args, 0)?),
            "abs" => return no_args(&args).map(|_| path::abs(ctx, p)),
            _ => {}
        }
    }
    match name {
        // `.feed(cmd)` (site/content/internals/values-streams-execution.md) spawns a child, which a pure value method
        // cannot do — the evaluator intercepts `.feed` in its method-call path
        // (shoal-eval `expr.rs::eval_feed`) before `call_method` is ever
        // reached, building an `ExecSpec` with the value's `feed_bytes` as
        // stdin. Reaching here means `.feed` was called through a path that
        // bypassed that bridge; surface a clear error rather than "no method".
        "feed" => Err(ErrorVal::type_error(
            ".feed must be evaluated by the interpreter, not as a pure value method",
        )),
        "len" | "count" => no_args(&args).and_then(|_| list::len(recv)),
        "is_empty" => no_args(&args)
            .and_then(|_| list::len(recv))
            .map(|v| Value::Bool(v == Value::Int(0))),
        "first" => list::first_last(recv, &args, true),
        "last" => list::first_last(recv, &args, false),
        "collect" => list::collect(recv),
        // `.stream()` promotes a finite collection (or a string's lines) into a
        // lazy `stream<T>` so the stream combinators (site/content/internals/streams-channels.md) can be exercised
        // on deterministic, in-memory data — the honest finite counterpart of the
        // live `watch`/`tail`/`every` sources.
        "stream" => no_args(&args).and_then(|_| list::to_stream(recv)),
        "tee" => list::tee(recv, int_arg(&args, 0, 2)?),
        "map" => list::map(ctx, recv, arg(&args, 0)?),
        "reduce" | "fold" => list::reduce(ctx, recv, arg(&args, 0)?.clone(), arg(&args, 1)?),
        "where" | "filter" => list::filter(ctx, recv, arg(&args, 0)?),
        "each" => list::each(ctx, recv, arg(&args, 0)?),
        "any" => list::any_all(ctx, recv, arg(&args, 0)?, true),
        "all" => list::any_all(ctx, recv, arg(&args, 0)?, false),
        "find" => list::find(ctx, recv, arg(&args, 0)?),
        "flat_map" => list::flat_map(ctx, recv, arg(&args, 0)?),
        "sort_by" => list::sort_by(ctx, recv, arg(&args, 0)?),
        // `sort(.key)` sorts by the key extractor (e.g. `ls.sort(.name)`);
        // `sort()` with no argument sorts the elements directly.
        "sort" => {
            if args.pos.is_empty() {
                list::sort(recv)
            } else {
                list::sort_by(ctx, recv, arg(&args, 0)?)
            }
        }
        "reverse" => list::reverse(recv),
        "uniq" => list::uniq(recv),
        // The zero-arg aggregates reject stray args LOUDLY. A projection lambda
        // (`[1,2,3].sum(x => x)`, `ls.sum(.size)`) was previously dropped
        // silently — a right-looking call with a wrong (or, for records, a
        // confusing `type_error`) answer. The correct idiom is `.map(f).sum()`,
        // which the error names. The bare `.sum`/`.min`/`.max` field→method
        // fallback passes no args, so it stays valid.
        "sum" => agg_no_args(&args, "sum").and_then(|_| list::sum(recv)),
        "min" => agg_no_args(&args, "min").and_then(|_| list::minmax(recv, false)),
        "max" => agg_no_args(&args, "max").and_then(|_| list::minmax(recv, true)),
        "flatten" => list::flatten(recv),
        "enumerate" => list::enumerate(recv),
        "skip" => list::slice_count(recv, int_arg(&args, 0, 0)?, false),
        "take" => list::slice_count(recv, int_arg(&args, 0, 0)?, true),
        "chunks" => list::chunks(recv, int_arg(&args, 0, 0)?),
        "zip" => list::zip(recv, arg(&args, 0)?.clone()),
        // `group_by` is an accepted alias for `group` (same {key, values}
        // table), added for consistency with `sort_by` — a dogfood pass found
        // agents reaching for `.group_by(.k)` by analogy and bouncing off
        // `unknown method .group_by` with a `did you mean .group()?` hint.
        "group" | "group_by" => list::group(ctx, recv, arg(&args, 0)?),
        // `.join()` deliberately defaults its separator to "" (plain
        // concatenation) — unlike the required-argument predicates below, a
        // zero-arg join has one obvious, harmless meaning.
        "join" => list::join(recv, str_arg(&args, 0, "")?),
        "lines" => strops::string_unary(recv, |s| {
            Value::List(
                s.lines()
                    .map(|x| Value::Str(x.trim_end_matches('\r').into()))
                    .collect(),
            )
        }),
        "words" => strops::string_unary(recv, |s| {
            Value::List(s.split_whitespace().map(|x| Value::Str(x.into())).collect())
        }),
        "chars" => strops::string_unary(recv, |s| {
            Value::List(s.chars().map(|x| Value::Str(x.to_string())).collect())
        }),
        "trim" => strops::string_unary(recv, |s| Value::Str(s.trim().into())),
        "upper" => strops::string_unary(recv, |s| Value::Str(s.to_uppercase())),
        "lower" => strops::string_unary(recv, |s| Value::Str(s.to_lowercase())),
        "split" => {
            let sep = req_str_arg(&args, 0, ".split requires a separator argument")?;
            strops::string_unary(recv, |s| {
                Value::List(s.split(sep).map(|x| Value::Str(x.into())).collect())
            })
        }
        "starts_with" => strops::string_pred(
            recv,
            req_str_arg(&args, 0, ".starts_with requires a prefix argument")?,
            |s, q| s.starts_with(q),
        ),
        "ends_with" => strops::string_pred(
            recv,
            req_str_arg(&args, 0, ".ends_with requires a suffix argument")?,
            |s, q| s.ends_with(q),
        ),
        "contains" => list::contains(recv, arg(&args, 0)?),
        "replace" => {
            let pat = arg(&args, 0)?.clone();
            let rep = req_str_arg(
                &args,
                1,
                ".replace requires a replacement argument: .replace(pattern, replacement)",
            )?;
            strops::replace_method(recv, &pat, rep)
        }
        "matches" => strops::matches_method(recv, arg(&args, 0)?),
        "match" => strops::match_method(recv, arg(&args, 0)?),
        "parse_int" => strops::string_parse(recv, "int"),
        "parse_float" => strops::string_parse(recv, "float"),
        "keys" => record::record_side(recv, true),
        "values" => record::record_side(recv, false),
        "items" => record::items(recv),
        "set" => record::set(
            recv,
            req_str_arg(&args, 0, ".set requires a key argument: .set(key, value)")?,
            arg(&args, 1)?.clone(),
        ),
        "merge" => record::merge(recv, arg(&args, 0)?.clone()),
        "get" => list::get(
            recv,
            arg(&args, 0)?,
            args.pos.get(1).cloned().unwrap_or(Value::Null),
        ),
        "str" => strops::to_str(recv, false),
        "display" => strops::to_str(recv, true),
        "json" => Ok(Value::Str(
            serde_json::to_string(&value_to_json(&recv)?)
                .map_err(|e| ErrorVal::new("custom", e.to_string()))?,
        )),
        "abs" => num::numeric_unary(recv, f64::abs, i64::checked_abs),
        "round" => num::round_to(recv, int_arg(&args, 0, 0)?, f64::round),
        "floor" => num::round_to(recv, int_arg(&args, 0, 0)?, f64::floor),
        "ceil" => num::round_to(recv, int_arg(&args, 0, 0)?, f64::ceil),
        "save" => path::save(ctx, recv, arg(&args, 0)?, false),
        "append" => path::save(ctx, recv, arg(&args, 0)?, true),
        // Task lifecycle methods (defect #14, site/content/internals/language-conformance-contract.md job control).
        "await" | "wait" => no_args(&args).and_then(|_| task::task_await(recv)),
        "cancel" => no_args(&args).and_then(|_| task::task_cancel(recv)),
        "is_done" => no_args(&args).and_then(|_| task::task_is_done(recv)),
        "suspend" => no_args(&args).and_then(|_| task::task_suspend(recv)),
        "resume" => no_args(&args).and_then(|_| task::task_resume(recv)),
        "is_suspended" => no_args(&args).and_then(|_| task::task_is_suspended(recv)),
        _ => Err(suggest::unknown_method(name, &recv)),
    }
}

pub(crate) fn arg(args: &CallArgs, n: usize) -> VResult<&Value> {
    args.pos
        .get(n)
        .ok_or_else(|| ErrorVal::arg_error(format!("missing argument {}", n + 1)))
}
pub(crate) fn no_args(args: &CallArgs) -> VResult<()> {
    if args.pos.is_empty() && args.named.is_empty() {
        Ok(())
    } else {
        Err(ErrorVal::arg_error("method takes no arguments"))
    }
}
/// `no_args` for the zero-arg aggregates (`sum`/`min`/`max`), whose classic
/// misuse is a projection lambda that a plain `arg(&args, 0)` would drop
/// silently. Names the method and points at the `.map(f).<agg>()` idiom.
pub(crate) fn agg_no_args(args: &CallArgs, name: &str) -> VResult<()> {
    if args.pos.is_empty() && args.named.is_empty() {
        Ok(())
    } else {
        Err(ErrorVal::arg_error(format!(
            "{name} takes no arguments (did you mean .map(f).{name}()?)"
        )))
    }
}
pub(crate) fn int_arg(args: &CallArgs, n: usize, default: i64) -> VResult<usize> {
    match args.pos.get(n) {
        None => Ok(default.max(0) as usize),
        Some(Value::Int(i)) if *i >= 0 => Ok(*i as usize),
        Some(v) => Err(ErrorVal::type_error(format!(
            "expected non-negative int, found {}",
            v.type_name()
        ))),
    }
}
pub(crate) fn str_arg<'a>(args: &'a CallArgs, n: usize, default: &'a str) -> VResult<&'a str> {
    match args.pos.get(n) {
        None => Ok(default),
        Some(Value::Str(s)) => Ok(s),
        Some(v) => Err(ErrorVal::type_error(format!(
            "expected str, found {}",
            v.type_name()
        ))),
    }
}
/// A REQUIRED `str` argument: a missing argument is an `arg_error`, never a
/// silent `""` default (lenient defaults made `"x".starts_with()` return
/// `true` — a predicate that lies).
pub(crate) fn req_str_arg<'a>(
    args: &'a CallArgs,
    n: usize,
    missing: &'static str,
) -> VResult<&'a str> {
    match args.pos.get(n) {
        None => Err(ErrorVal::arg_error(missing)),
        Some(Value::Str(s)) => Ok(s),
        Some(v) => Err(ErrorVal::type_error(format!(
            "expected str, found {}",
            v.type_name()
        ))),
    }
}

#[cfg(test)]
mod tests;
