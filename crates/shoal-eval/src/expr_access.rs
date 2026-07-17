//! Field/index access, `.feed`, and the iterable-conversion helpers (see
//! [`crate::expr`] for the split rationale).

use super::*;
use std::io::Read as _;

/// Maximum chunk admitted to the bounded child-stdin queue. Combined with the
/// 16-slot queue below, stream feed owns at most 1 MiB of queued byte buffers,
/// independent of the size of a resident or CAS-backed item.
const STREAM_STDIN_CHUNK_BYTES: usize = 64 * 1024;

impl Evaluator {
    /// Resolve `v.name` as a *field* — the direct, no-fallback accessor set.
    /// Callers that want the `.field` sugar to also reach zero-arg methods (so
    /// `.map(.upper)` / `path.read` work) go through `field_or_method`, which
    /// falls back to method dispatch on `field_missing`. Borrows `v` so that
    /// fallback can still own it.
    pub(crate) fn field(&self, v: &Value, name: &str) -> VResult<Value> {
        match v {
            Value::Record(r) => r
                .get(name)
                .cloned()
                .ok_or_else(|| ErrorVal::new("field_missing", format!("missing field `{name}`"))),
            Value::Outcome(o) => match name {
                "status" => Ok(o.status.map_or(Value::Null, |x| Value::Int(x as i64))),
                "ok" => Ok(Value::Bool(o.ok)),
                "signal" => Ok(o.signal.clone().map_or(Value::Null, Value::Str)),
                "dur" => Ok(Value::Duration(o.dur_ns)),
                "pid" => Ok(Value::Int(o.pid as i64)),
                "cmd" => Ok(Value::Str(o.cmd.clone())),
                // Raw stream bytes are always reachable, even on failure. A
                // spilled capture surfaces here as lazy, ref-backed
                // `bytes` (true `.len`, on-demand load); ordinary output is the
                // resident `bytes` exactly as before.
                "stdout" => Ok(o.stdout_value()),
                "stderr" => Ok(Value::Bytes(o.stderr.clone())),
                "out" | "err" if !o.ok => Err(ErrorVal::new(
                    "cmd_failed",
                    match (o.status, &o.signal) {
                        (Some(code), _) => format!("`{}` exited with status {code}", o.cmd),
                        (_, Some(signal)) => format!("`{}` died from {signal}", o.cmd),
                        _ => format!("`{}` failed", o.cmd),
                    },
                )
                .with_hint(String::from_utf8_lossy(&o.stderr).trim().to_string())),
                "out" => Ok(o.out_value()),
                "err" => Ok(Value::Bytes(o.stderr.clone())),
                // Outcome unification (P1b): an unknown field forwards to the
                // structured `.out` — `(echo hi).out` is direct, but
                // `(stat f).size` / `outcome.name` resolve against `.out` too.
                // A failed outcome raises the same `cmd_failed` as `.out`.
                _ if o.ok => self.field(&o.out_value(), name),
                _ => Err(ErrorVal::new(
                    "cmd_failed",
                    match (o.status, &o.signal) {
                        (Some(code), _) => format!("`{}` exited with status {code}", o.cmd),
                        (_, Some(signal)) => format!("`{}` died from {signal}", o.cmd),
                        _ => format!("`{}` failed", o.cmd),
                    },
                )
                .with_hint(String::from_utf8_lossy(&o.stderr).trim().to_string())),
            },
            // A caught `error` value (bound by `catch err { … }`) exposes its
            // parts so a handler can branch on them —
            // `catch err { if err.code == "not_found" { … } }`. Mirrors the
            // `ErrorVal` fields; absent optionals read as `null`.
            Value::Error(e) => match name {
                "code" => Ok(Value::Str(e.code.clone())),
                "msg" => Ok(Value::Str(e.msg.clone())),
                "hint" => Ok(e.hint.clone().map_or(Value::Null, Value::Str)),
                "stderr" => Ok(e.stderr.clone().map_or(Value::Null, Value::Str)),
                "status" => Ok(e.status.map_or(Value::Null, |s| Value::Int(s as i64))),
                _ => Err(ErrorVal::new(
                    "field_missing",
                    format!("error has no field `{name}`"),
                )),
            },
            // Calendar-component fields on a datetime (backs `now.year`, the
            // site/content/internals/language-conformance-contract.md relative-anchor probe, and tagged-literal access).
            Value::DateTime(z) => match name {
                "year" => Ok(Value::Int(z.year() as i64)),
                "month" => Ok(Value::Int(z.month() as i64)),
                "day" => Ok(Value::Int(z.day() as i64)),
                "hour" => Ok(Value::Int(z.hour() as i64)),
                "minute" => Ok(Value::Int(z.minute() as i64)),
                "second" => Ok(Value::Int(z.second() as i64)),
                _ => Err(ErrorVal::new(
                    "field_missing",
                    format!("datetime has no field `{name}`"),
                )),
            },
            // Duration relative-anchor composition (site/content/internals/language-conformance-contract.md): `30d.ago` /
            // `1h.from_now` resolve against the live wall clock into a datetime.
            Value::Duration(ns) => match name {
                "ago" | "from_now" => {
                    let base = crate::helpers::now_zoned(self.host.clock.as_ref());
                    let signed = if name == "ago" { -*ns } else { *ns };
                    let span = jiff::SignedDuration::from_nanos(signed);
                    base.checked_add(span)
                        .map(|z| Value::DateTime(Box::new(z)))
                        .map_err(|e| {
                            ErrorVal::new("overflow", format!("datetime out of range: {e}"))
                        })
                }
                _ => Err(ErrorVal::new(
                    "field_missing",
                    format!("duration has no field `{name}`"),
                )),
            },
            // A glob VALUE exposes its source `.pattern` (site/content/internals/intercrate-protocol-contracts.md);
            // its matches are reached with `.expand()` or any collection method.
            Value::Glob(g) => match name {
                "pattern" => Ok(Value::Str(g.pattern.clone())),
                _ => Err(ErrorVal::new(
                    "field_missing",
                    format!("glob has no field `{name}`"),
                )),
            },
            // A path's zero-arg accessors double as fields so the `.field`
            // shorthand in implicit lambdas reaches them — `glob("*.rs").map(.name)`,
            // `ls.where(.size > 1mb)`, `glob("*.toml").map(.read.parse_toml())`.
            // Pure components resolve without IO; the fs-backed accessors route
            // through the `Fs` port via `path_fs_method` (site/content/internals/intercrate-protocol-contracts.md).
            // Only the argument-taking methods (`.join`/`.abs`/`.save`/`.append`)
            // stay method-only — a bare `.field` can't carry their argument.
            Value::Path(p) => {
                let component = |part: Option<&std::ffi::OsStr>| match part {
                    Some(s) => Value::Str(s.to_string_lossy().into_owned()),
                    None => Value::Null,
                };
                match name {
                    "name" => Ok(component(p.file_name())),
                    "stem" => Ok(component(p.file_stem())),
                    "ext" => Ok(component(p.extension())),
                    "parent" => Ok(match p.parent() {
                        Some(par) if !par.as_os_str().is_empty() => Value::Path(par.to_path_buf()),
                        _ => Value::Null,
                    }),
                    "read" | "read_bytes" | "lines" | "exists" | "is_dir" | "is_file" | "size"
                    | "modified" => self.path_fs_method(p, name),
                    _ => Err(ErrorVal::new(
                        "field_missing",
                        format!("path has no field `{name}`"),
                    )),
                }
            }
            _ => Err(ErrorVal::new(
                "field_missing",
                format!("{} has no field `{name}`", v.type_name()),
            )),
        }
    }
    /// `v.name` with the `.field`→zero-arg-method fallback: try field access
    /// first; on `field_missing`, dispatch `name` as a zero-arg method so the
    /// bare-`.field` sugar reaches methods too — `names.map(.upper)`,
    /// `path.read`, `{a:1}.json`. A present field always wins over a
    /// same-named method (user data ahead of the stdlib), and the original
    /// `field_missing` is preferred when the method is also absent so the
    /// error still names the field, not the method.
    pub(crate) fn field_or_method(&mut self, v: Value, name: &str, span: Span) -> VResult<Value> {
        match self.field(&v, name) {
            // A Record's field namespace is user-controlled, so field access on
            // a record is STRICT: a `.name` that misses a field raises the loud
            // `field_missing` and must NOT silently fall through to a same-named
            // builtin method. Otherwise any record whose intended field collides
            // with a method (`items`/`keys`/`values`/`json`/`len`/…) — e.g.
            // `{key: "a", values: [1,2,3]}.items` — would return the generic
            // record method's result, shadowing real data with stdlib behaviour
            // (a silent-wrong answer). Call a record method explicitly with
            // parens (`record.items()`), which routes through normal method
            // dispatch. Non-record receivers (str/path/int/list/glob/…) have no
            // user-controlled field names to shadow, so they KEEP the fallback —
            // that is what makes `.map(.upper)` (str→method), `.map(.name)`
            // (path accessor), and `[1,2,3].sum` (list→method) resolve.
            Err(e) if e.code == "field_missing" && !matches!(v, Value::Record(_)) => {
                match self.dispatch_method(v, name, &Args::empty(), span) {
                    Err(me) if me.code == "field_missing" => Err(e),
                    other => other,
                }
            }
            other => other,
        }
    }

    /// Dispatch a method call `v.name(args)` across the full method surface:
    /// the evaluator-hosted specials (`.pick`, channel ops, stream sinks, the
    /// filesystem-backed path methods, glob-as-collection, and callable record
    /// fields) and, failing those, the pure `shoal_value::methods` stdlib.
    /// Shared by `Expr::MethodCall` and the `.field` fallback above.
    pub(crate) fn dispatch_method(
        &mut self,
        v: Value,
        name: &str,
        args: &Args,
        span: Span,
    ) -> VResult<Value> {
        // A bare `val:blake3:<hash>` content ref written as a value (the
        // short-ref form `.ref` yields) is resolvable in-language (site/content/internals/language-conformance-contract.md
        // follow-up): load its bytes from this session's journal CAS and
        // re-dispatch on the resulting lazy `bytes`, so a recovered ref answers
        // `.len`, materializes, etc. exactly like the capture it came from. A
        // string that is NOT a content ref falls straight through to normal
        // string-method dispatch; an unresolvable one (no CAS, unknown hash)
        // surfaces a clear `not_found` rather than a wrong string-length answer.
        if let Value::Str(s) = &v
            && let Some(resolved) = self.resolve_content_ref(s, span)
        {
            return self.dispatch_method(resolved?, name, args, span);
        }
        if name == "pick" {
            // Wired to shoal-picker here (not methods.rs) to avoid a
            // shoal-value → shoal-picker dependency cycle.
            let a = self.eval_args(args)?;
            self.pick(v, &a).map_err(|e| e.or_span(span))
        } else if let Some(chan) = crate::channels::as_channel(&v)
            && matches!(name, "emit" | "events" | "latest" | "take")
        {
            // In-language `channel(name)` ops (site/content/internals/streams-channels.md): wired
            // here (not methods.rs) because they reach the session event bus,
            // which shoal-value cannot see.
            let chan = chan.to_string();
            let a = self.eval_args(args)?;
            self.eval_channel_method(&chan, name, a)
                .map_err(|e| e.or_span(span))
        } else if matches!(v, Value::Stream(_)) && matches!(name, "into" | "render") {
            // Stream sinks that need the evaluator (the event bus for
            // `.into(channel)`, the statement sink for `.render()`).
            let a = self.eval_args(args)?;
            self.eval_stream_sink(v, name, a)
                .map_err(|e| e.or_span(span))
        } else if let Value::Path(p) = &v
            && matches!(
                name,
                "read"
                    | "read_bytes"
                    | "lines"
                    | "exists"
                    | "is_dir"
                    | "is_file"
                    | "size"
                    | "modified"
            )
        {
            // Filesystem-backed path methods (site/content/internals/intercrate-protocol-contracts.md) route
            // through the evaluator's Fs port, resolving against cwd. They
            // take no arguments.
            if !args.pos.is_empty() || !args.named.is_empty() {
                return Err(
                    ErrorVal::arg_error(format!(".{name} takes no arguments")).or_span(span)
                );
            }
            let p = p.clone();
            self.path_fs_method(&p, name).map_err(|e| e.or_span(span))
        } else if let Value::Glob(g) = &v {
            // A glob VALUE behaves as a lazy collection of its matches
            // (site/content/internals/language-conformance-contract.md): `.pattern`/`.expand()` are glob-native; every other
            // method expands the glob to a sorted `list<path>` and re-dispatches
            // on that list, so `glob("*.rs").map(…)`, `.len()`, `.first(3)`, etc.
            // all work. (Passing a glob AS a command argument still expands at
            // the callee — unchanged.)
            match name {
                "pattern" => {
                    if !args.pos.is_empty() || !args.named.is_empty() {
                        return Err(
                            ErrorVal::arg_error(".pattern takes no arguments").or_span(span)
                        );
                    }
                    Ok(Value::Str(g.pattern.clone()))
                }
                "expand" => {
                    if !args.pos.is_empty() || !args.named.is_empty() {
                        return Err(ErrorVal::arg_error(".expand takes no arguments").or_span(span));
                    }
                    Ok(Value::List(self.expand_glob(g)?))
                }
                _ => {
                    let list = Value::List(self.expand_glob(g)?);
                    let a = self.eval_args(args)?;
                    shoal_value::methods::call_method(self, list, name, a, span)
                }
            }
        } else if let Value::Record(r) = &v
            && r.get(name).is_some_and(Value::is_callable)
        {
            // A callable record field is invoked as a method — this is how a
            // module fn runs as `deploy.build(...)` (site/content/internals/roadmap-and-priorities.md modules) and how
            // any record-of-closures dispatches.
            let f = r.get(name).cloned().expect("callable field present");
            let a = self.eval_args(args)?;
            self.call_value(&f, a).map_err(|e| e.or_span(span))
        } else {
            let a = self.eval_args(args)?;
            shoal_value::methods::call_method(self, v, name, a, span)
        }
    }

    pub(crate) fn index(&self, v: Value, idx: Value) -> VResult<Value> {
        match (v, idx) {
            (Value::List(xs), Value::Int(i)) => {
                let n = if i < 0 { xs.len() as i64 + i } else { i };
                xs.get(n as usize)
                    .cloned()
                    .ok_or_else(|| ErrorVal::new("index_range", "list index out of range"))
            }
            (Value::Str(s), Value::Int(i)) => {
                let cs = s.chars().collect::<Vec<_>>();
                let n = if i < 0 { cs.len() as i64 + i } else { i };
                cs.get(n as usize)
                    .map(|c| Value::Str(c.to_string()))
                    .ok_or_else(|| ErrorVal::new("index_range", "string index out of range"))
            }
            (Value::Record(mut r), Value::Str(k)) => r
                .shift_remove(&k)
                .ok_or_else(|| ErrorVal::new("field_missing", format!("missing field `{k}`"))),
            (a, b) => Err(ErrorVal::new(
                "type_error",
                format!("cannot index {} with {}", a.type_name(), b.type_name()),
            )),
        }
    }
    /// Filesystem-backed `path` methods (`.read`/`.read_bytes`/`.lines`/
    /// `.exists`/`.is_dir`/`.is_file`/`.size`/`.modified`, site/content/internals/intercrate-protocol-contracts.md).
    /// These live in the evaluator rather than `shoal-value::methods` because
    /// they perform IO — routed through the [`Fs`] port so a fake can interpose
    /// — and resolve relative paths against the session cwd.
    pub(crate) fn path_fs_method(&self, p: &Path, name: &str) -> VResult<Value> {
        let abs = if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.exec.shell.cwd.join(p)
        };
        let ioerr = |e: std::io::Error| {
            let code = if e.kind() == std::io::ErrorKind::NotFound {
                "not_found"
            } else {
                "custom"
            };
            ErrorVal::new(code, format!("{}: {e}", abs.display()))
        };
        let utf8err = || ErrorVal::new("utf8_error", format!("{}: not valid UTF-8", abs.display()));
        match name {
            "read" => {
                let bytes = self.host.fs.read(&abs).map_err(ioerr)?;
                String::from_utf8(bytes)
                    .map(Value::Str)
                    .map_err(|_| utf8err())
            }
            "read_bytes" => Ok(Value::Bytes(Arc::new(
                self.host.fs.read(&abs).map_err(ioerr)?,
            ))),
            "lines" => {
                let bytes = self.host.fs.read(&abs).map_err(ioerr)?;
                let s = String::from_utf8(bytes).map_err(|_| utf8err())?;
                Ok(Value::List(
                    s.lines()
                        .map(|l| Value::Str(l.trim_end_matches('\r').into()))
                        .collect(),
                ))
            }
            "exists" => Ok(Value::Bool(self.host.fs.metadata(&abs).is_ok())),
            "is_dir" => Ok(Value::Bool(
                self.host
                    .fs
                    .metadata(&abs)
                    .map(|m| m.is_dir())
                    .unwrap_or(false),
            )),
            "is_file" => Ok(Value::Bool(
                self.host
                    .fs
                    .metadata(&abs)
                    .map(|m| m.is_file())
                    .unwrap_or(false),
            )),
            "size" => Ok(Value::Size(
                self.host.fs.metadata(&abs).map_err(ioerr)?.len(),
            )),
            "modified" => {
                let m = self.host.fs.metadata(&abs).map_err(ioerr)?;
                Ok(m.modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .and_then(|d| jiff::Timestamp::from_nanosecond(d.as_nanos() as i128).ok())
                    .map(|ts| Value::DateTime(Box::new(ts.to_zoned(jiff::tz::TimeZone::system()))))
                    .unwrap_or(Value::Null))
            }
            _ => unreachable!("path_fs_method called with unexpected name `{name}`"),
        }
    }

    pub(crate) fn values_from(&mut self, v: Value) -> VResult<Vec<Value>> {
        match v {
            Value::List(xs) => Ok(xs),
            Value::Table(rs) => Ok(rs.into_iter().map(Value::Record).collect()),
            Value::Range(r) => Ok(r.iter().map(Value::Int).collect()),
            // Iterating a glob VALUE expands its matches (site/content/internals/language-conformance-contract.md): `for f in
            // glob("*.rs")` walks the sorted `list<path>`. (Passing a glob as a
            // command argument still expands at the callee — that is unchanged.)
            Value::Glob(g) => self.expand_glob(&g),
            // Iterating a stream in a `for` drives it to completion (site/content/internals/streams-channels.md);
            // an endless stream errors `stream_unbounded` — use `.each(f)` for
            // those, or bound it with `.take`/`.take_until` first.
            Value::Stream(s) => shoal_value::collect_stream(self, &s),
            _ => Err(ErrorVal::new("type_error", "value is not iterable")),
        }
    }

    /// `.feed` (site/content/internals/values-streams-execution.md): pipe a value's serialized bytes into a command's
    /// stdin, returning the command's outcome. Handles both spellings —
    /// `value.feed(cmd)` (canonical) and `cmd.feed(value)` (inverted) — by
    /// classifying which operand is the command node.
    pub(crate) fn eval_feed(
        &mut self,
        recv: &Expr,
        args: &Args,
        position: Position,
        span: Span,
    ) -> VResult<Value> {
        if args.pos.len() != 1 || !args.named.is_empty() {
            return Err(ErrorVal::arg_error(
                ".feed expects exactly one command argument",
            ));
        }
        let arg = &args.pos[0];
        // Inverted `cmd.feed(value)`: the receiver is the command node, the
        // argument the value. Canonical `value.feed(cmd)`: the other way round.
        let (value_expr, cmd_expr) = if self.is_command_expr(recv) {
            (arg, recv)
        } else {
            (recv, arg)
        };
        let value = self.eval_expr(value_expr, Position::Value)?;
        if let Value::Stream(stream) = value {
            return self.eval_stream_feed(stream, cmd_expr, position, span);
        }
        let bytes = shoal_value::feed_bytes(&value).map_err(|e| e.or_span(span))?;
        self.eval_feed_command(cmd_expr, position, span, StdinSpec::Bytes(bytes))
    }

    fn eval_stream_feed(
        &mut self,
        stream: shoal_value::StreamVal,
        cmd_expr: &Expr,
        position: Position,
        span: Span,
    ) -> VResult<Value> {
        const STDIN_CHUNKS: usize = 16;
        const PULL_POLL: Duration = Duration::from_millis(25);

        let mut upstream = stream.take_upstream()?;
        let (stdin_sink, stdin) = shoal_exec::stream_stdin(STDIN_CHUNKS);
        let pump_cancel = CancelToken::new();
        let parent_cancel = self.cancellation_token();
        let child = self.child_context();
        let error = Arc::new(std::sync::Mutex::new(None));
        let pump_error = error.clone();
        let worker_cancel = pump_cancel.clone();
        let worker = std::thread::spawn(move || {
            let mut evaluator = child.build(ChildKind::StreamPump, parent_cancel.clone());
            loop {
                if worker_cancel.is_cancelled() || parent_cancel.is_cancelled() {
                    break;
                }
                let item = match upstream.pull(&mut evaluator, Some(PULL_POLL)) {
                    Ok(shoal_value::Pull::Item(value)) => value,
                    Ok(shoal_value::Pull::Timeout) => continue,
                    Ok(shoal_value::Pull::End) => break,
                    Err(e) => {
                        *pump_error.lock().unwrap() = Some(e);
                        break;
                    }
                };
                let sent =
                    match send_stream_item(&stdin_sink, &worker_cancel, &parent_cancel, &item) {
                        Ok(sent) => sent,
                        Err(e) => {
                            *pump_error.lock().unwrap() = Some(e);
                            break;
                        }
                    };
                if !sent {
                    break;
                }
            }
        });

        let result = self.eval_feed_command(cmd_expr, position, span, stdin);
        // Wake a pump blocked on an idle live upstream or a full stdin queue
        // once the process has exited or failed to spawn.
        pump_cancel.cancel();
        if worker.join().is_err() {
            return Err(ErrorVal::new("custom", "stream stdin pump panicked").with_span(span));
        }
        if let Some(e) = error.lock().unwrap().take() {
            return Err(e.or_span(span));
        }
        result
    }

    fn eval_feed_command(
        &mut self,
        cmd_expr: &Expr,
        position: Position,
        span: Span,
        stdin: StdinSpec,
    ) -> VResult<Value> {
        match cmd_expr {
            Expr::LangBlock { tool, src, span } => {
                self.eval_lang_block(tool, src, stdin, position, *span)
            }
            Expr::Cmd { call, .. } => {
                let mut argv = vec![OsString::from(&call.head)];
                for a in &call.args {
                    for v in self.expand_arg(a)? {
                        argv.push(self.argv_value(v)?);
                    }
                }
                self.run_argv(argv, position, stdin, &call.env_prefix, call.span, None)
            }
            Expr::Var { name, .. } => {
                self.run_argv(vec![OsString::from(name)], position, stdin, &[], span, None)
            }
            other => Err(ErrorVal::type_error(format!(
                ".feed's argument must be a command, not {}",
                expr_noun(other)
            ))
            .with_hint(
                "feed to a command name (with args if needed: `.feed(sort -r)`), \
                 an `sh { … }` block, or an interpreter block (`.feed(jq { … })`)",
            )
            .with_span(span)),
        }
    }

    /// Is `e` a command-shaped node — an interpreter block, an explicit command
    /// call, or a bare name that is not a bound variable (a command head)? Used
    /// by `.feed` to tell `cmd.feed(value)` from `value.feed(cmd)`.
    fn is_command_expr(&self, e: &Expr) -> bool {
        match e {
            Expr::LangBlock { .. } | Expr::Cmd { .. } => true,
            Expr::Var { name, .. } => self.exec.shell.env.get(name).is_none(),
            _ => false,
        }
    }
}

fn send_stream_item(
    sink: &StdinSink,
    stop: &CancelToken,
    parent_cancel: &CancelToken,
    value: &Value,
) -> VResult<bool> {
    match value {
        Value::Bytes(bytes) => {
            return Ok(send_stdin_bytes(
                sink,
                stop,
                parent_cancel,
                bytes.as_slice(),
            ));
        }
        Value::CasBytes(bytes) => {
            let mut reader = bytes.open()?;
            loop {
                let mut chunk = vec![0u8; STREAM_STDIN_CHUNK_BYTES];
                let n = reader
                    .read(&mut chunk)
                    .map_err(|e| ErrorVal::new("io_error", format!("stream feed: {e}")))?;
                if n == 0 {
                    return Ok(true);
                }
                chunk.truncate(n);
                if !send_stdin_chunk(sink, stop, parent_cancel, chunk) {
                    return Ok(false);
                }
            }
        }
        _ => {}
    }

    let raw_chunk = matches!(value, Value::Outcome(_));
    let mut bytes = shoal_value::feed_bytes(value)?;
    // Streams of values are line-framed so item boundaries survive the byte
    // boundary (tail strings become lines; records become NDJSON). Explicit
    // byte/outcome chunks remain raw for binary and already-framed producers.
    if !raw_chunk && !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    Ok(send_stdin_bytes(sink, stop, parent_cancel, &bytes))
}

fn send_stdin_bytes(
    sink: &StdinSink,
    stop: &CancelToken,
    parent_cancel: &CancelToken,
    bytes: &[u8],
) -> bool {
    for chunk in bytes.chunks(STREAM_STDIN_CHUNK_BYTES) {
        if !send_stdin_chunk(sink, stop, parent_cancel, chunk.to_vec()) {
            return false;
        }
    }
    true
}

fn send_stdin_chunk(
    sink: &StdinSink,
    stop: &CancelToken,
    parent_cancel: &CancelToken,
    mut chunk: Vec<u8>,
) -> bool {
    loop {
        if stop.is_cancelled() || parent_cancel.is_cancelled() {
            return false;
        }
        match sink.try_send(chunk) {
            Ok(()) => return true,
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return false,
            Err(std::sync::mpsc::TrySendError::Full(returned)) => {
                chunk = returned;
                std::thread::park_timeout(Duration::from_millis(2));
            }
        }
    }
}

/// A short noun for an expression, for `.feed`'s diagnostic.
fn expr_noun(e: &Expr) -> &'static str {
    match e {
        Expr::Str { .. } | Expr::StrInterp { .. } => "a string",
        Expr::Int { .. } | Expr::Float { .. } => "a number",
        Expr::List { .. } => "a list",
        Expr::Record { .. } => "a record",
        _ => "that value",
    }
}
