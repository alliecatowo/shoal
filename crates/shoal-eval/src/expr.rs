//! Expression evaluation: `eval_expr`'s dispatch over every `Expr` form, the
//! `&&`/`||` short-circuit chain, and the field/index/iterable-conversion
//! helpers expression evaluation leans on.

use super::*;

impl Evaluator {
    pub fn eval_expr(&mut self, expr: &Expr, position: Position) -> VResult<Value> {
        let span = expr.span();
        let result = match expr {
            Expr::Null { .. } => Ok(Value::Null),
            Expr::Bool { value, .. } => Ok(Value::Bool(*value)),
            Expr::Int { value, .. } => Ok(Value::Int(*value)),
            Expr::Float { value, .. } => Ok(Value::Float(*value)),
            Expr::Str { value, .. } => Ok(Value::Str(value.clone())),
            Expr::Size { bytes, .. } => Ok(Value::Size(*bytes)),
            Expr::Duration { ns, .. } => Ok(Value::Duration(*ns)),
            Expr::Time { hour, min, sec, .. } => Ok(Value::Time(shoal_value::TimeVal {
                hour: *hour,
                min: *min,
                sec: *sec,
            })),
            Expr::Regex { src, .. } => {
                Ok(Value::Regex(Arc::new(shoal_value::RegexVal::compile(src)?)))
            }
            Expr::DateTime { iso, .. } => {
                crate::helpers::parse_datetime(iso).map(|z| Value::DateTime(Box::new(z)))
            }
            Expr::Var { name, span } => {
                // A name that isn't a variable but *is* a command resolves by
                // invoking it zero-arg in value position (defect #5, §3.4).
                if let Some(v) = self.env.get(name) {
                    Ok(v)
                } else if self.is_command_name(name) {
                    let call = CmdCall {
                        head: name.clone(),
                        forced: false,
                        args: vec![],
                        redirects: vec![],
                        env_prefix: vec![],
                        background: false,
                        trailing: None,
                        span: *span,
                    };
                    self.eval_command(&call, Position::Value)
                } else {
                    Err(ErrorVal::new(
                        "undefined_var",
                        format!("undefined variable `{name}`"),
                    ))
                }
            }
            Expr::StrInterp { parts, .. } => {
                let mut out = String::new();
                for p in parts {
                    match p {
                        StrPart::Lit { text } => out.push_str(text),
                        StrPart::Expr { expr } => {
                            let v = self.eval_expr(expr, Position::Value)?;
                            if matches!(v, Value::Secret(_)) {
                                return Err(ErrorVal::new(
                                    "type_error",
                                    "secrets cannot be interpolated",
                                ));
                            }
                            match v {
                                Value::Str(s) => out.push_str(&s),
                                _ => out.push_str(&shoal_value::render::render_inline(&v)),
                            }
                        }
                    }
                }
                Ok(Value::Str(out))
            }
            Expr::List { items, .. } => items
                .iter()
                .map(|e| self.eval_expr(e, Position::Value))
                .collect::<VResult<Vec<_>>>()
                .map(Value::List),
            Expr::Record { fields, .. } => {
                let mut r = Record::new();
                for f in fields {
                    r.insert(f.name.clone(), self.eval_expr(&f.value, Position::Value)?);
                }
                Ok(Value::Record(r))
            }
            Expr::Unary { op, expr, .. } => {
                let v = self.eval_expr(expr, Position::Value)?;
                match (op, v) {
                    (UnOp::Not, v) => Ok(Value::Bool(!v.as_condition()?)),
                    (UnOp::Neg, Value::Int(i)) => i
                        .checked_neg()
                        .map(Value::Int)
                        .ok_or_else(|| ErrorVal::new("overflow", "integer negation overflow")),
                    (UnOp::Neg, Value::Float(f)) => Ok(Value::Float(-f)),
                    (UnOp::Neg, Value::Duration(n)) => n
                        .checked_neg()
                        .map(Value::Duration)
                        .ok_or_else(|| ErrorVal::new("overflow", "duration negation overflow")),
                    (_, v) => Err(ErrorVal::new(
                        "type_error",
                        format!("cannot apply unary operator to {}", v.type_name()),
                    )),
                }
            }
            Expr::Binary {
                op: BinOp::And | BinOp::Or,
                ..
            } => self.eval_chain(expr, position == Position::Statement),
            Expr::Binary {
                op: BinOp::Coalesce,
                lhs,
                rhs,
                ..
            } => {
                let l = self.eval_expr(lhs, Position::Value)?;
                if l == Value::Null {
                    self.eval_expr(rhs, Position::Value)
                } else {
                    Ok(l)
                }
            }
            Expr::Binary { op, lhs, rhs, .. } => {
                let l = self.eval_expr(lhs, Position::Value)?;
                let r = self.eval_expr(rhs, Position::Value)?;
                shoal_value::ops::binop(*op, &l, &r)
            }
            Expr::Range {
                start,
                end,
                inclusive,
                ..
            } => match (
                self.eval_expr(start, Position::Value)?,
                self.eval_expr(end, Position::Value)?,
            ) {
                (Value::Int(a), Value::Int(b)) => Ok(Value::Range(shoal_value::RangeVal {
                    start: a,
                    end: b,
                    inclusive: *inclusive,
                })),
                _ => Err(ErrorVal::new("type_error", "range bounds must be int")),
            },
            Expr::Field {
                recv,
                name,
                optional,
                ..
            } => {
                let v = self.eval_expr(recv, Position::Value)?;
                if *optional && v == Value::Null {
                    Ok(Value::Null)
                } else {
                    self.field(v, name)
                }
            }
            Expr::Index { recv, index, .. } => {
                let v = self.eval_expr(recv, Position::Value)?;
                let i = self.eval_expr(index, Position::Value)?;
                self.index(v, i)
            }
            Expr::MethodCall {
                recv,
                name,
                args,
                optional,
                ..
            } => {
                if name == "feed" {
                    return self.eval_feed(recv, args, position, span);
                }
                if matches!(&**recv, Expr::Var { name, .. } if name == "secret") && name == "get" {
                    let args = self.eval_args(args)?;
                    let [Value::Str(secret_name)] = args.pos.as_slice() else {
                        return Err(ErrorVal::arg_error("secret.get expects one string name"));
                    };
                    let home = std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from("."));
                    let dir = std::env::var_os("SHOAL_SECRET_DIR")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| {
                            std::env::var_os("XDG_DATA_HOME")
                                .map(PathBuf::from)
                                .unwrap_or_else(|| home.join(".local/share"))
                                .join("shoal/secrets")
                        });
                    let store = shoal_secret::SecretStore::open(dir)
                        .map_err(|e| ErrorVal::new("permission", e.to_string()))?;
                    let value = store
                        .get(secret_name)
                        .map_err(|e| ErrorVal::new("permission", e.to_string()))?
                        .ok_or_else(|| {
                            ErrorVal::new("not_found", format!("secret `{secret_name}` not found"))
                        })?;
                    let text = String::from_utf8(value.to_vec()).map_err(|_| {
                        ErrorVal::new(
                            "utf8_error",
                            "secret is not valid UTF-8 for environment injection",
                        )
                    })?;
                    return Ok(Value::Secret(shoal_value::SecretVal {
                        name: secret_name.clone(),
                        value: Arc::from(text),
                    }));
                }
                let v = self.eval_expr(recv, Position::Value)?;
                if *optional && v == Value::Null {
                    Ok(Value::Null)
                } else if name == "pick" {
                    // Wired to shoal-picker here (not methods.rs) to avoid a
                    // shoal-value → shoal-picker dependency cycle.
                    let a = self.eval_args(args)?;
                    self.pick(v, &a).map_err(|e| e.or_span(span))
                } else if let Some(chan) = crate::channels::as_channel(&v)
                    && matches!(name.as_str(), "emit" | "events" | "latest" | "take")
                {
                    // In-language `channel(name)` ops (docs/STREAMS.md §2.5, §7):
                    // wired here (not methods.rs) because they reach the session
                    // event bus, which shoal-value cannot see.
                    let chan = chan.to_string();
                    let a = self.eval_args(args)?;
                    self.eval_channel_method(&chan, name, a)
                        .map_err(|e| e.or_span(span))
                } else if matches!(v, Value::Stream(_))
                    && matches!(name.as_str(), "into" | "render")
                {
                    // Stream sinks that need the evaluator (the event bus for
                    // `.into(channel)`, the statement sink for `.render()`).
                    let a = self.eval_args(args)?;
                    self.eval_stream_sink(v, name, a)
                        .map_err(|e| e.or_span(span))
                } else {
                    let args = self.eval_args(args)?;
                    shoal_value::methods::call_method(self, v, name, args, span)
                }
            }
            Expr::FnCall { name, args, .. } => {
                // Structured builtins that take closures/thunks (§5).
                match name.as_str() {
                    "parallel" => return self.builtin_parallel(args),
                    "retry" => return self.builtin_retry(args),
                    "on" => return self.builtin_on(args),
                    "run" => {
                        let mut a = self.eval_args(args)?;
                        if a.pos.is_empty() {
                            return Err(ErrorVal::arg_error("run expects a path or command name"));
                        }
                        let target = a.pos.remove(0);
                        return self.run_poly(target, a.pos, position);
                    }
                    "save" => {
                        let a = self.eval_args(args)?;
                        return self.builtin_save(a.pos);
                    }
                    "open" => {
                        let a = self.eval_args(args)?;
                        return self.builtin_open(a.pos);
                    }
                    _ => {}
                }
                let a = self.eval_args(args)?;
                if let Some(value) = self.call_constructor(name, &a)? {
                    return Ok(value);
                }
                if let Some(f) = self.env.get(name) {
                    return self.call_value(&f, a);
                }
                // A name that isn't a fn but *is* a command resolves by invoking
                // it with the given args in value position (defect #5).
                if self.is_command_name(name) {
                    let mut call = CmdCall {
                        head: name.clone(),
                        forced: false,
                        args: vec![],
                        redirects: vec![],
                        env_prefix: vec![],
                        background: false,
                        trailing: None,
                        span,
                    };
                    for v in a.pos {
                        call.args.push(self.value_cmd_arg(v, span)?);
                    }
                    for (n, v) in a.named {
                        call.args.push(CmdArg::FlagLong {
                            name: n,
                            value: Some(Box::new(self.value_cmd_arg(v, span)?)),
                            span,
                        });
                    }
                    return self.eval_command(&call, Position::Value);
                }
                Err(ErrorVal::new(
                    "undefined_var",
                    format!("undefined function `{name}`"),
                ))
            }
            Expr::Lambda { params, body, .. } => Ok(Value::Closure(Arc::new(ClosureVal {
                name: None,
                params: params.clone(),
                rest: None,
                ret: None,
                body: *body.clone(),
                env: self.env.clone(),
                doc: None,
            }))),
            Expr::Block { block, .. } => match self.eval_block(block, false)? {
                Flow::Value(v) | Flow::Return(v) => Ok(v),
                Flow::Break | Flow::Continue => {
                    Err(ErrorVal::new("custom", "loop control outside loop"))
                }
            },
            Expr::If {
                cond, then, r#else, ..
            } => {
                if self.eval_expr(cond, Position::Value)?.as_condition()? {
                    self.block_value(then)
                } else {
                    match r#else {
                        Some(e) => self.eval_expr(e, Position::Value),
                        None => Ok(Value::Null),
                    }
                }
            }
            Expr::Try {
                body,
                pattern,
                handler,
                ..
            } => match self.block_value(body) {
                Ok(v) => Ok(v),
                Err(e) => {
                    let old = self.env.clone();
                    self.env = old.child();
                    if let Some(p) = pattern {
                        self.bind_pattern(p, Value::Error(Arc::new(e)), false)?;
                    }
                    let r = self.block_value(handler);
                    self.env = old;
                    r
                }
            },
            Expr::Catch {
                expr,
                binder,
                handler,
                ..
            } => match self.eval_expr(expr, Position::Value) {
                Ok(v) => Ok(v),
                Err(e) => {
                    let old = self.env.clone();
                    self.env = old.child();
                    if let Some(n) = binder {
                        self.env
                            .declare(n.clone(), Value::Error(Arc::new(e)), false);
                    }
                    let r = self.eval_expr(handler, Position::Value);
                    self.env = old;
                    r
                }
            },
            Expr::Cmd { call, .. } => self.eval_command(call, position),
            Expr::LangBlock { tool, src, .. } => {
                self.eval_lang_block(tool, src, StdinSpec::Null, position, span)
            }
            Expr::With {
                cwd,
                env,
                reef,
                body,
                ..
            } => self.eval_with(cwd.as_deref(), env.as_deref(), reef.as_deref(), body),
            Expr::Spawn { body, .. } => self.spawn_block(body.clone()),
            Expr::Match {
                scrutinee, arms, ..
            } => self.eval_match(scrutinee, arms),
        };
        result.map_err(|e| e.or_span(span))
    }

    /// Evaluate an `&&`/`||` chain (outcome unification, P1d). Per the normative
    /// corpus (`spec/cases/outcome.toml`, TDD §1.10/§3.3/§4.5) the operators are
    /// NOT bool-narrowing: they return the short-circuiting operand **verbatim**
    /// (whichever side's `as_condition()` decided the result), so a chain of
    /// outcome commands stays chainable — `(echo a && echo b).status` still
    /// works. Operands run in *value* position so a failed command surfaces as
    /// an outcome the chain short-circuits on rather than raising (letting
    /// `sh{exit 1} || echo x` recover). When `emit` (statement/discard context)
    /// every executed command operand's output is routed to the sink EXCEPT the
    /// returned one (the caller renders that once), so `echo a && echo b` prints
    /// both and an arbitrarily long chain prints every stage.
    pub(crate) fn eval_chain(&mut self, e: &Expr, emit: bool) -> VResult<Value> {
        let Expr::Binary {
            op: op @ (BinOp::And | BinOp::Or),
            lhs,
            rhs,
            span,
        } = e
        else {
            // Leaf: an ordinary sub-expression (a command, a bool, …).
            return self.eval_expr(e, Position::Value);
        };
        let l = self.eval_chain(lhs, emit)?;
        let ok = l.as_condition().map_err(|err| err.or_span(*span))?;
        let short = match op {
            BinOp::And => !ok,
            BinOp::Or => ok,
            _ => unreachable!(),
        };
        if short {
            // The short-circuiting operand decides — returned verbatim, not sunk
            // here (the caller renders it once).
            Ok(l)
        } else {
            // `l` is no longer the returned operand: print it if it was a
            // command outcome, then the rhs decides.
            if emit && crate::helpers::is_command_expr(lhs) {
                self.sink_value(&l);
            }
            self.eval_chain(rhs, emit)
        }
    }

    pub(crate) fn field(&self, v: Value, name: &str) -> VResult<Value> {
        match v {
            Value::Record(mut r) => r
                .shift_remove(name)
                .ok_or_else(|| ErrorVal::new("field_missing", format!("missing field `{name}`"))),
            Value::Outcome(o) => match name {
                "status" => Ok(o.status.map_or(Value::Null, |x| Value::Int(x as i64))),
                "ok" => Ok(Value::Bool(o.ok)),
                "signal" => Ok(o.signal.clone().map_or(Value::Null, Value::Str)),
                "dur" => Ok(Value::Duration(o.dur_ns)),
                "pid" => Ok(Value::Int(o.pid as i64)),
                "cmd" => Ok(Value::Str(o.cmd.clone())),
                // Raw stream bytes are always reachable, even on failure.
                "stdout" => Ok(Value::Bytes(o.stdout.clone())),
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
                _ if o.ok => self.field(o.out_value(), name),
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
            _ => Err(ErrorVal::new(
                "field_missing",
                format!("{} has no field `{name}`", v.type_name()),
            )),
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
    pub(crate) fn values_from(&mut self, v: Value) -> VResult<Vec<Value>> {
        match v {
            Value::List(xs) => Ok(xs),
            Value::Table(rs) => Ok(rs.into_iter().map(Value::Record).collect()),
            Value::Range(r) => Ok(r.iter().map(Value::Int).collect()),
            // Iterating a stream in a `for` drives it to completion (STREAMS §4);
            // an endless stream errors `stream_unbounded` — use `.each(f)` for
            // those, or bound it with `.take`/`.take_until` first.
            Value::Stream(s) => shoal_value::collect_stream(self, &s),
            _ => Err(ErrorVal::new("type_error", "value is not iterable")),
        }
    }

    /// Evaluate an interpreter block (IO.md §2.6): resolve `tool` as a command
    /// and hand it `src` as its program via the tool's inline-eval convention
    /// (`lang_block_invocation`). `stdin` is whatever `.feed` supplies (or
    /// `Null` for a bare block) — it stays a separate channel from the program,
    /// so `.feed` composes with the block.
    pub(crate) fn eval_lang_block(
        &mut self,
        tool: &str,
        src: &str,
        stdin: StdinSpec,
        position: Position,
        span: Span,
    ) -> VResult<Value> {
        let (tail, stdin_src) = lang_block_invocation(tool, src);
        // A tool whose convention is "program on stdin" cannot also accept fed
        // bytes — the two would collide on the single stdin channel.
        let stdin = match (stdin_src, &stdin) {
            (Some(_), StdinSpec::Bytes(_) | StdinSpec::File(_)) => {
                return Err(ErrorVal::type_error(format!(
                    "`{tool}` takes its program on stdin, so it cannot also be fed data"
                ))
                .with_span(span));
            }
            (Some(bytes), _) => StdinSpec::Bytes(bytes),
            (None, _) => stdin,
        };
        let mut argv = vec![OsString::from(tool)];
        argv.extend(tail);
        self.run_argv(argv, position, stdin, &[], span, None)
    }

    /// `.feed` (IO.md §1): pipe a value's serialized bytes into a command's
    /// stdin, returning the command's outcome. Handles both spellings —
    /// `value.feed(cmd)` (canonical) and `cmd.feed(value)` (inverted) — by
    /// classifying which operand is the command node.
    fn eval_feed(
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
        let bytes = shoal_value::feed_bytes(&value).map_err(|e| e.or_span(span))?;
        match cmd_expr {
            Expr::LangBlock { tool, src, span } => {
                self.eval_lang_block(tool, src, StdinSpec::Bytes(bytes), position, *span)
            }
            Expr::Cmd { call, .. } => {
                let mut argv = vec![OsString::from(&call.head)];
                for a in &call.args {
                    for v in self.expand_arg(a)? {
                        argv.push(self.argv_value(v)?);
                    }
                }
                self.run_argv(
                    argv,
                    position,
                    StdinSpec::Bytes(bytes),
                    &call.env_prefix,
                    call.span,
                    None,
                )
            }
            Expr::Var { name, .. } => self.run_argv(
                vec![OsString::from(name)],
                position,
                StdinSpec::Bytes(bytes),
                &[],
                span,
                None,
            ),
            other => Err(ErrorVal::type_error(format!(
                ".feed's argument must be a command, not {}",
                expr_noun(other)
            ))
            .with_span(span)),
        }
    }

    /// Is `e` a command-shaped node — an interpreter block, an explicit command
    /// call, or a bare name that is not a bound variable (a command head)? Used
    /// by `.feed` to tell `cmd.feed(value)` from `value.feed(cmd)`.
    fn is_command_expr(&self, e: &Expr) -> bool {
        match e {
            Expr::LangBlock { .. } | Expr::Cmd { .. } => true,
            Expr::Var { name, .. } => self.env.get(name).is_none(),
            _ => false,
        }
    }
}

/// Map an interpreter-class tool to how its `src` program reaches it (IO.md
/// §2.6 step 3): the argv tail after the resolved binary, plus `Some(bytes)`
/// when the program must instead go on stdin (the default for an
/// interpreter-classed tool with no inline-eval flag). This is the *only* place
/// a `-c`-shaped flag is spelled, and it is data, never typed by the user.
pub fn lang_block_invocation(tool: &str, src: &str) -> (Vec<OsString>, Option<Vec<u8>>) {
    let flag = |f: &str| (vec![OsString::from(f), OsString::from(src)], None);
    match tool {
        "sh" | "bash" | "zsh" | "fish" | "python" | "python3" => flag("-c"),
        "node" | "ruby" | "perl" | "lua" | "Rscript" | "osascript" => flag("-e"),
        "php" => flag("-r"),
        "deno" => (vec![OsString::from("eval"), OsString::from(src)], None),
        "jq" => (vec![OsString::from(src)], None),
        _ => (vec![], Some(src.as_bytes().to_vec())),
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

#[cfg(test)]
mod lang_block_tests {
    use super::lang_block_invocation;
    use std::ffi::OsString;

    fn os(v: &[&str]) -> Vec<OsString> {
        v.iter().map(OsString::from).collect()
    }

    #[test]
    fn c_flag_family() {
        for tool in ["sh", "bash", "zsh", "fish", "python", "python3"] {
            let (tail, stdin) = lang_block_invocation(tool, "BODY");
            assert_eq!(tail, os(&["-c", "BODY"]), "{tool}");
            assert!(stdin.is_none(), "{tool}");
        }
    }

    #[test]
    fn e_flag_family() {
        for tool in ["node", "ruby", "perl", "lua", "Rscript", "osascript"] {
            assert_eq!(
                lang_block_invocation(tool, "X").0,
                os(&["-e", "X"]),
                "{tool}"
            );
        }
    }

    #[test]
    fn special_forms() {
        assert_eq!(lang_block_invocation("php", "X").0, os(&["-r", "X"]));
        assert_eq!(lang_block_invocation("deno", "X").0, os(&["eval", "X"]));
        // jq: filter as the sole arg, data left for stdin.
        assert_eq!(lang_block_invocation("jq", ".a").0, os(&[".a"]));
    }

    #[test]
    fn unmapped_interpreter_feeds_program_on_stdin() {
        let (tail, stdin) = lang_block_invocation("wat", "prog");
        assert!(tail.is_empty());
        assert_eq!(stdin.as_deref(), Some(b"prog".as_slice()));
    }
}
