//! Method dispatch for `stream<T>` (site/content/internals/streams-channels.md). Lazy combinators return a
//! fresh stream (consuming `s`, site/content/internals/language-conformance-contract.md); sinks drive it. `.into` / `.render` /
//! `.feed` are handled one level up in the evaluator (they need the event bus,
//! the statement sink, or a child process) and never reach here.

use super::*;
use std::io::Write as _;

pub(crate) fn stream_method(
    ctx: &mut dyn CallCtx,
    s: StreamVal,
    name: &str,
    args: CallArgs,
) -> VResult<Value> {
    let stream = Value::Stream;
    match name {
        // --- lazy combinators (return a new stream) ---
        "map" => s.map(arg(&args, 0)?.clone()).map(stream),
        "where" | "filter" => s.filter(arg(&args, 0)?.clone()).map(stream),
        "scan" => s
            .scan(arg(&args, 0)?.clone(), arg(&args, 1)?.clone())
            .map(stream),
        "flat_map" => s.flat_map(arg(&args, 0)?.clone()).map(stream),
        "take" => s.take_n(int_arg(&args, 0, 0)?).map(stream),
        "take_until" => match arg(&args, 0)? {
            Value::Stream(o) => s.take_until_stream(o.clone()).map(stream),
            f => s.take_until_pred(f.clone()).map(stream),
        },
        "dedupe" => no_args(&args).and_then(|_| s.dedupe()).map(stream),
        "distinct" => no_args(&args).and_then(|_| s.distinct()).map(stream),
        "debounce" => s.debounce(dur_arg(&args, 0)?).map(stream),
        "throttle" => s.throttle(dur_arg(&args, 0)?).map(stream),
        "window" => match arg(&args, 0)? {
            Value::Duration(ns) if *ns >= 0 => s
                .window_dur(std::time::Duration::from_nanos(*ns as u64))
                .map(stream),
            Value::Int(n) if *n > 0 => s.window_count(*n as usize).map(stream),
            _ => Err(ErrorVal::arg_error(
                "window expects a positive count or a duration",
            )),
        },
        "buffer" => {
            let capacity = int_arg(&args, 0, 1)?;
            ctx.buffer_stream(s, capacity).map(stream)
        }
        "enumerate" => no_args(&args).and_then(|_| s.enumerate()).map(stream),
        "merge" => match arg(&args, 0)? {
            Value::Stream(o) => s.merge(o.clone()).map(stream),
            v => Err(ErrorVal::type_error(format!(
                "merge expects a stream, found {}",
                v.type_name()
            ))),
        },
        "zip" => match arg(&args, 0)? {
            Value::Stream(o) => s.zip(o.clone()).map(stream),
            v => Err(ErrorVal::type_error(format!(
                "zip expects a stream, found {}",
                v.type_name()
            ))),
        },
        // --- sinks (drive the stream) ---
        "each" => {
            let f = arg(&args, 0)?.clone();
            let mut up = s.take_upstream()?;
            drive_stream(ctx, &mut *up, |ctx, v| {
                ctx.call_closure(&f, vec![v])?;
                Ok(())
            })?;
            Ok(Value::Null)
        }
        "collect" => no_args(&args).and_then(|_| collect_stream(ctx, &s).map(Value::List)),
        "save" | "append" => stream_save(ctx, s, arg(&args, 0)?),
        // `.tee(n)` (site/content/internals/streams-channels.md): fork into n independently-drivable streams.
        // A bounded stream materializes once and each fork replays the full
        // list (exact whole-stream replay, the pre-existing behavior); a
        // live/endless stream — where materializing would be
        // `stream_unbounded` — forks lazily over the shared source with
        // bounded per-fork queues (`stream/tee.rs`).
        "tee" => {
            let n = int_arg(&args, 0, 2)?;
            if s.is_bounded() {
                super::list::tee(Value::List(collect_stream(ctx, &s)?), n)
            } else {
                Ok(Value::List(
                    s.tee(n)?.into_iter().map(Value::Stream).collect(),
                ))
            }
        }
        // Everything else (`.sort`, `.sum`, `.uniq`, `.first`, `.len`, …)
        // is a collection op: materialize the *bounded* stream, then dispatch.
        _ => {
            let list = Value::List(collect_stream(ctx, &s)?);
            super::dispatch(ctx, list, name, args)
        }
    }
}

/// `.save(path)` / `.append(path)` on a stream (site/content/internals/streams-channels.md): append each item as
/// it arrives (live logging) rather than buffering the whole stream first.
fn stream_save(ctx: &mut dyn CallCtx, s: StreamVal, path: &Value) -> VResult<Value> {
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
    // Open once through the injected `Fs` port instead of `std::fs::OpenOptions`
    // (HR-C2): the sink keeps its open-once / append-each-item streaming shape,
    // but the write now crosses the same enforceable boundary as `path.read`.
    // `ctx.fs()` borrows `ctx` only for this call and hands back an owned
    // writer, so `drive_stream` below can still take `ctx` mutably.
    let mut file = ctx
        .fs()
        .open_append(&p)
        .map_err(|e| ErrorVal::new("custom", format!("{}: {e}", p.display())))?;
    let mut up = s.take_upstream()?;
    drive_stream(ctx, &mut *up, |_ctx, v| {
        let mut bytes = value_line_bytes(&v)?;
        bytes.push(b'\n');
        file.write_all(&bytes)
            .map_err(|e| ErrorVal::new("custom", format!("{}: {e}", p.display())))
    })?;
    Ok(Value::Path(p))
}

/// One stream item rendered to its on-disk line form (str/bytes verbatim, other
/// values as JSON — matching `.save`'s serialization).
fn value_line_bytes(v: &Value) -> VResult<Vec<u8>> {
    Ok(match v {
        Value::Str(s) => s.as_bytes().to_vec(),
        Value::Bytes(b) => (**b).clone(),
        _ => serde_json::to_vec(&value_to_json(v))
            .map_err(|e| ErrorVal::new("custom", e.to_string()))?,
    })
}

fn dur_arg(args: &CallArgs, n: usize) -> VResult<std::time::Duration> {
    match args.pos.get(n) {
        Some(Value::Duration(ns)) if *ns >= 0 => Ok(std::time::Duration::from_nanos(*ns as u64)),
        _ => Err(ErrorVal::arg_error("expected a non-negative duration")),
    }
}
