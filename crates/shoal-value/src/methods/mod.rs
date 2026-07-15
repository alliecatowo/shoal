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
//! - [`stream`] — the `stream<T>` method surface (STREAMS §3–§4).
//! - [`outcome`] — outcome method forwarding (P1b unification).
//! - [`task`] — task lifecycle methods (TDD §4.7 job control).
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
    // Outcome unification (P1b): an unknown method on a command outcome forwards
    // to its structured `.out`, so `ls.where(.size > 1b).sort(.name)` works
    // (`ls` is an outcome; `.where`/`.sort` operate on its `.out` table). Raw
    // stream bytes stay reachable via `.stdout`/`.stderr`.
    if let Value::Outcome(o) = &recv {
        return outcome::forward(ctx, o, name, args);
    }
    // Streams (docs/STREAMS.md) get their own method surface: the lazy
    // combinators (§3) return a NEW stream without driving the source, and the
    // sinks (§4) drive it (with `ctx` for closure stages). Anything else falls
    // through to the collection methods by materializing a *bounded* stream to a
    // list first (an unbounded stream errors `stream_unbounded`).
    if let Value::Stream(s) = recv {
        return stream::stream_method(ctx, s, name, args);
    }
    // Pure (no-IO) `path` component accessors (docs/CONTRACTS.md §3). Intercepted
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
        // `.feed(cmd)` (IO.md §1) spawns a child, which a pure value method
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
        // lazy `stream<T>` so the stream combinators (STREAMS §3) can be exercised
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
        "sum" => list::sum(recv),
        "min" => list::minmax(recv, false),
        "max" => list::minmax(recv, true),
        "flatten" => list::flatten(recv),
        "enumerate" => list::enumerate(recv),
        "skip" => list::slice_count(recv, int_arg(&args, 0, 0)?, false),
        "take" => list::slice_count(recv, int_arg(&args, 0, 0)?, true),
        "chunks" => list::chunks(recv, int_arg(&args, 0, 0)?),
        "zip" => list::zip(recv, arg(&args, 0)?.clone()),
        "group" => list::group(ctx, recv, arg(&args, 0)?),
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
            serde_json::to_string(&value_to_json(&recv))
                .map_err(|e| ErrorVal::new("custom", e.to_string()))?,
        )),
        "abs" => num::numeric_unary(recv, f64::abs, i64::checked_abs),
        "round" => num::round_to(recv, int_arg(&args, 0, 0)?, f64::round),
        "floor" => num::round_to(recv, int_arg(&args, 0, 0)?, f64::floor),
        "ceil" => num::round_to(recv, int_arg(&args, 0, 0)?, f64::ceil),
        "save" => path::save(ctx, recv, arg(&args, 0)?, false),
        "append" => path::save(ctx, recv, arg(&args, 0)?, true),
        // Task lifecycle methods (defect #14, TDD §4.7 job control).
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
    fn task_suspend_resume_methods() {
        let t = crate::TaskVal::new("t");
        assert_eq!(
            call(Value::Task(t.clone()), "is_suspended", vec![]).unwrap(),
            Value::Bool(false)
        );
        // `.suspend()` returns the task (chainable) and flips the flag.
        assert!(matches!(
            call(Value::Task(t.clone()), "suspend", vec![]).unwrap(),
            Value::Task(_)
        ));
        assert!(t.is_suspended());
        assert_eq!(
            call(Value::Task(t.clone()), "is_suspended", vec![]).unwrap(),
            Value::Bool(true)
        );
        call(Value::Task(t.clone()), "resume", vec![]).unwrap();
        assert!(!t.is_suspended());
        // Suspend/resume hooks fire.
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let f = flag.clone();
        t.on_suspend(Box::new(move || {
            f.store(true, std::sync::atomic::Ordering::SeqCst)
        }));
        t.suspend();
        assert!(flag.load(std::sync::atomic::Ordering::SeqCst));
        // Wrong receiver type is a type error.
        assert_eq!(
            call(Value::Int(1), "suspend", vec![]).unwrap_err().code,
            "type_error"
        );
    }

    #[test]
    fn path_pure_component_methods() {
        let p = || Value::Path(PathBuf::from("/a/b/file.tar.gz"));
        assert_eq!(
            call(p(), "name", vec![]).unwrap(),
            Value::Str("file.tar.gz".into())
        );
        assert_eq!(
            call(p(), "stem", vec![]).unwrap(),
            Value::Str("file.tar".into())
        );
        assert_eq!(call(p(), "ext", vec![]).unwrap(), Value::Str("gz".into()));
        assert_eq!(
            call(p(), "parent", vec![]).unwrap(),
            Value::Path(PathBuf::from("/a/b"))
        );
        assert_eq!(
            call(p(), "join", vec![Value::Str("x".into())]).unwrap(),
            Value::Path(PathBuf::from("/a/b/file.tar.gz/x"))
        );
        // An extensionless / rootless path yields nulls where appropriate.
        assert_eq!(
            call(Value::Path(PathBuf::from("README")), "ext", vec![]).unwrap(),
            Value::Null
        );
        assert_eq!(
            call(Value::Path(PathBuf::from("/")), "parent", vec![]).unwrap(),
            Value::Null
        );
        // `.abs()` absolutizes a relative path against the ctx cwd.
        let cwd = std::env::temp_dir();
        assert_eq!(
            call(Value::Path(PathBuf::from("rel/x")), "abs", vec![]).unwrap(),
            Value::Path(cwd.join("rel/x"))
        );
        // `.str()` remains the fallible converter, still reaching a path.
        assert_eq!(
            call(Value::Path(PathBuf::from("/a/b")), "str", vec![]).unwrap(),
            Value::Str("/a/b".into())
        );
    }

    #[test]
    fn unknown_method_carries_did_you_mean_hint() {
        let list = Value::List(vec![Value::Int(1)]);
        let e = call(list.clone(), "length", vec![]).unwrap_err();
        assert_eq!(e.code, "field_missing");
        assert_eq!(e.hint.as_deref(), Some("did you mean .len()?"));
        let e = call(list.clone(), "size", vec![]).unwrap_err();
        assert_eq!(e.hint.as_deref(), Some("did you mean .len()?"));
        let e = call(Value::Str("a".into()), "to_upper", vec![]).unwrap_err();
        assert_eq!(e.hint.as_deref(), Some("did you mean .upper()?"));
        let e = call(Value::Path(PathBuf::from("x")), "read_str", vec![]).unwrap_err();
        assert_eq!(e.hint.as_deref(), Some("did you mean .read()?"));
        let e = call(list.clone(), "push", vec![Value::Int(2)]).unwrap_err();
        assert!(e.hint.unwrap().contains("immutable"));
        let e = call(Value::Str("ab".into()), "substring", vec![Value::Int(1)]).unwrap_err();
        assert!(e.hint.unwrap().contains(".take"));
        // A near-typo resolves by edit distance.
        let e = call(list, "sortt", vec![]).unwrap_err();
        assert_eq!(e.hint.as_deref(), Some("did you mean .sort()?"));
        // Nothing plausible → no hint, same error as before.
        let e = call(Value::Int(1), "frobnicate", vec![]).unwrap_err();
        assert_eq!(e.code, "field_missing");
        assert_eq!(e.hint, None);
    }

    #[test]
    fn scalar_str_renders_canonical_form() {
        assert_eq!(
            call(Value::Int(42), "str", vec![]).unwrap(),
            Value::Str("42".into())
        );
        assert_eq!(
            call(Value::Float(1.5), "str", vec![]).unwrap(),
            Value::Str("1.5".into())
        );
        assert_eq!(
            call(Value::Bool(true), "str", vec![]).unwrap(),
            Value::Str("true".into())
        );
        // Unconverted types keep erroring, now with a teaching hint.
        let e = call(Value::List(vec![]), "str", vec![]).unwrap_err();
        assert_eq!(e.code, "type_error");
        assert!(e.hint.unwrap().contains("interpolation"));
    }

    #[test]
    fn required_str_args_error_when_missing() {
        let s = || Value::Str("hello".into());
        for (method, args) in [
            ("starts_with", vec![]),
            ("ends_with", vec![]),
            ("split", vec![]),
            ("replace", vec![Value::Str("l".into())]),
        ] {
            let e = call(s(), method, args).unwrap_err();
            assert_eq!(e.code, "arg_error", "{method} must require its argument");
        }
        let e = call(Value::Record(Record::new()), "set", vec![]).unwrap_err();
        assert_eq!(e.code, "arg_error");
        assert!(e.msg.contains("key"));
        // Explicit empty-string arguments are still legal.
        assert_eq!(
            call(s(), "starts_with", vec![Value::Str("".into())]).unwrap(),
            Value::Bool(true)
        );
        // `.join()` keeps its deliberate "" default (concatenation).
        assert_eq!(
            call(
                Value::List(vec![Value::Str("a".into()), Value::Str("b".into())]),
                "join",
                vec![]
            )
            .unwrap(),
            Value::Str("ab".into())
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
