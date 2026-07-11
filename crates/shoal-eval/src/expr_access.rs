//! Field/index access, `.feed`, and the iterable-conversion helpers (see
//! [`crate::expr`] for the split rationale).

use super::*;

impl Evaluator {
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

    /// `.feed` (IO.md §1): pipe a value's serialized bytes into a command's
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
