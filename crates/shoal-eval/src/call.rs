//! Call machinery: evaluating call-argument lists, invoking closures/CmdRefs,
//! the builtin type-constructor forms (`path(...)`, `glob(...)`, `regex(...)`),
//! and `.pick()`.

use super::*;

impl Evaluator {
    pub(crate) fn eval_args(&mut self, args: &Args) -> VResult<CallArgs> {
        Ok(CallArgs {
            pos: args
                .pos
                .iter()
                .map(|e| self.eval_expr(e, Position::Value))
                .collect::<VResult<_>>()?,
            named: args
                .named
                .iter()
                .map(|a| Ok((a.name.clone(), self.eval_expr(&a.value, Position::Value)?)))
                .collect::<VResult<_>>()?,
        })
    }

    pub(crate) fn call_value(&mut self, f: &Value, args: CallArgs) -> VResult<Value> {
        // Runtime recursion guard (defect #9): unbounded native recursion aborts
        // the process, so cap the interpreter call stack well below that.
        self.exec.call_depth += 1;
        if self.exec.call_depth > 10_000 {
            self.exec.call_depth -= 1;
            return Err(ErrorVal::new(
                "recursion_limit",
                "recursion limit exceeded (10000 nested calls)",
            ));
        }
        let r = self.call_value_inner(f, args);
        self.exec.call_depth -= 1;
        r
    }

    pub(crate) fn call_value_inner(&mut self, f: &Value, args: CallArgs) -> VResult<Value> {
        match f {
            Value::Closure(c) => {
                // Strict binding (site/content/internals/language-conformance-contract.md: "arity/type errors carry the exact
                // source span"): excess positionals only flow into a declared
                // `...rest`; without one they are an arg_error, as is any
                // named argument that names no parameter. Silently dropping
                // either mis-binds without a diagnostic.
                if c.rest.is_none() && args.pos.len() > c.params.len() {
                    return Err(ErrorVal::new(
                        "arg_error",
                        format!(
                            "expected {} argument{}, got {}",
                            c.params.len(),
                            if c.params.len() == 1 { "" } else { "s" },
                            args.pos.len()
                        ),
                    ));
                }
                if let Some((name, _)) = args
                    .named
                    .iter()
                    .find(|(n, _)| !c.params.iter().any(|p| &p.name == n))
                {
                    return Err(ErrorVal::new(
                        "arg_error",
                        format!("unknown argument `{name}`"),
                    ));
                }
                let old = self.exec.env.clone();
                self.exec.env = c.env.child();
                for (i, p) in c.params.iter().enumerate() {
                    let val = args
                        .named
                        .iter()
                        .find(|(n, _)| n == &p.name)
                        .map(|x| x.1.clone())
                        .or_else(|| args.pos.get(i).cloned());
                    let val = match (val, &p.default) {
                        (Some(v), _) => v,
                        (None, Some(d)) => self.eval_expr(d, Position::Value)?,
                        _ => {
                            self.exec.env = old;
                            return Err(ErrorVal::new(
                                "arg_error",
                                format!("missing argument `{}`", p.name),
                            ));
                        }
                    };
                    // A `list<T>` annotation coerces per element (site/content/internals/language-conformance-contract.md
                    // site 2) on the EXPR path too; CMD calls arrive with
                    // word lists pre-coerced, so this is idempotent there.
                    let val = match crate::coerce::coerce_list_param(val, p.ty.as_ref()) {
                        Ok(v) => v,
                        Err(e) => {
                            self.exec.env = old;
                            return Err(e);
                        }
                    };
                    self.exec.env.declare(p.name.clone(), val, false);
                }
                if let Some(rest) = &c.rest {
                    self.exec.env.declare(
                        rest.name.clone(),
                        Value::List(args.pos.iter().skip(c.params.len()).cloned().collect()),
                        false,
                    );
                }
                // Track fn-body nesting so `cd`/env writes can be rejected (#10).
                self.exec.in_fn_body += 1;
                let out = self.eval_expr(&c.body, Position::Value);
                self.exec.in_fn_body -= 1;
                self.exec.env = old;
                out
            }
            Value::CmdRef(call) => {
                let mut call = (**call).clone();
                for v in args.pos {
                    call.args.push(self.value_cmd_arg(v, call.span)?);
                }
                // Later flags append to the aliased call's argv too (site/content/internals/language-conformance-contract.md):
                // `alias gs = git status; gs --short` must carry `--short`
                // through, not drop it. A bare presence flag (`--short`) arrives
                // as `Bool(true)`; a valued flag (`--n=5`) carries its value.
                for (name, v) in args.named {
                    let value = match v {
                        Value::Bool(true) => None,
                        other => Some(Box::new(self.value_cmd_arg(other, call.span)?)),
                    };
                    call.args.push(CmdArg::FlagLong {
                        name,
                        value,
                        span: call.span,
                    });
                }
                self.eval_command(&call, Position::Value)
            }
            _ => Err(ErrorVal::new(
                "type_error",
                format!("{} is not callable", f.type_name()),
            )),
        }
    }

    /// `assert(cond: bool, msg: str?)` (site/content/internals/intercrate-protocol-contracts.md): raise `assert_failed`
    /// with `msg` (or a default) when `cond` is false, else `null`.
    pub(crate) fn builtin_assert(&self, args: &CallArgs) -> VResult<Value> {
        if !args.named.is_empty() {
            return Err(ErrorVal::arg_error("assert takes no named arguments"));
        }
        let cond = args
            .pos
            .first()
            .ok_or_else(|| ErrorVal::arg_error("assert expects a condition"))?;
        if args.pos.len() > 2 {
            return Err(ErrorVal::arg_error(
                "assert expects at most a condition and a message",
            ));
        }
        if cond.as_condition()? {
            return Ok(Value::Null);
        }
        let msg = match args.pos.get(1) {
            Some(Value::Str(s)) => s.clone(),
            Some(v) => {
                return Err(ErrorVal::type_error(format!(
                    "assert message must be a str, found {}",
                    v.type_name()
                )));
            }
            None => "assertion failed".to_string(),
        };
        Err(ErrorVal::new("assert_failed", msg))
    }

    pub(crate) fn call_constructor(&self, name: &str, args: &CallArgs) -> VResult<Option<Value>> {
        let one = || {
            if !args.named.is_empty() || args.pos.len() != 1 {
                Err(ErrorVal::new(
                    "arg_error",
                    format!("{name} expects exactly one positional argument"),
                ))
            } else {
                Ok(&args.pos[0])
            }
        };
        match name {
            "path" => match one()? {
                Value::Str(s) => Ok(Some(Value::Path(PathBuf::from(s)))),
                Value::Path(p) => Ok(Some(Value::Path(p.clone()))),
                v => Err(ErrorVal::new(
                    "type_error",
                    format!("path expects str, found {}", v.type_name()),
                )),
            },
            "glob" => match args.pos.as_slice() {
                [Value::Str(pattern)]
                    if args
                        .named
                        .iter()
                        .all(|(name, _)| name == "hidden" || name == "follow") =>
                {
                    Ok(Some(Value::Glob(shoal_value::GlobVal {
                        pattern: pattern.clone(),
                        cwd: self.exec.cwd.clone(),
                        hidden: args
                            .named
                            .iter()
                            .find(|(name, _)| name == "hidden")
                            .is_some_and(|(_, value)| *value == Value::Bool(true)),
                    })))
                }
                [v] if !matches!(v, Value::Str(_)) => Err(ErrorVal::new(
                    "type_error",
                    format!("glob expects str, found {}", v.type_name()),
                )),
                _ => Err(ErrorVal::new(
                    "arg_error",
                    "glob expects one pattern and optional hidden/follow arguments",
                )),
            },
            "regex" => match one()? {
                Value::Str(src) => Ok(Some(Value::Regex(Arc::new(
                    shoal_value::RegexVal::compile(src)?,
                )))),
                v => Err(ErrorVal::new(
                    "type_error",
                    format!("regex expects str, found {}", v.type_name()),
                )),
            },
            // Stream sources + in-language channels (site/content/internals/streams-channels.md). All
            // yield a lazy `stream<T>` (channels via `.events()`); `channel(name)`
            // itself yields a handle whose `.emit/.events/.latest/.take` the
            // evaluator intercepts.
            "channel" => match one()? {
                Value::Str(name) => Ok(Some(crate::channels::channel_handle(name))),
                v => Err(ErrorVal::type_error(format!(
                    "channel expects a str name, found {}",
                    v.type_name()
                ))),
            },
            "every" => match one()? {
                Value::Duration(ns) if *ns >= 0 => Ok(Some(
                    self.source_every(std::time::Duration::from_nanos(*ns as u64))?,
                )),
                v => Err(ErrorVal::type_error(format!(
                    "every expects a duration, found {}",
                    v.type_name()
                ))),
            },
            "watch" => {
                if args.pos.len() != 1 {
                    return Err(ErrorVal::arg_error("watch expects one path or glob"));
                }
                let recursive = args
                    .get_named("recursive")
                    .map(|v| *v == Value::Bool(true))
                    .unwrap_or(true);
                Ok(Some(self.source_watch(&args.pos[0], recursive)?))
            }
            "tail" => {
                if args.pos.len() != 1 {
                    return Err(ErrorVal::arg_error("tail expects one file path"));
                }
                let from_start = args
                    .get_named("from_start")
                    .map(|v| *v == Value::Bool(true))
                    .unwrap_or(false);
                Ok(Some(self.source_tail(&args.pos[0], from_start)?))
            }
            _ => Ok(None),
        }
    }

    /// `.pick()` — interactive fuzzy selection via shoal-picker, gated on a tty.
    pub(crate) fn pick(&self, recv: Value, args: &CallArgs) -> VResult<Value> {
        let multi = args
            .named
            .iter()
            .find(|(n, _)| n == "multi")
            .map(|(_, v)| matches!(v, Value::Bool(true)))
            .unwrap_or(false);
        let prompt = args
            .named
            .iter()
            .find(|(n, _)| n == "prompt")
            .and_then(|(_, v)| match v {
                Value::Str(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "> ".into());
        let options = shoal_picker::Options {
            multi,
            prompt,
            ..Default::default()
        };
        let selected = shoal_picker::pick(recv, options).map_err(|e| match e.kind() {
            std::io::ErrorKind::Unsupported => ErrorVal::arg_error("pick needs a terminal"),
            std::io::ErrorKind::Interrupted => ErrorVal::new("custom", "pick cancelled"),
            _ => ErrorVal::new("custom", e.to_string()),
        })?;
        if multi {
            Ok(Value::List(selected))
        } else {
            Ok(selected.into_iter().next().unwrap_or(Value::Null))
        }
    }
}
