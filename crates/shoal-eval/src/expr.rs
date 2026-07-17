//! Expression evaluation: `eval_expr`'s dispatch over every `Expr` form and
//! the interpreter-block/`.feed` plumbing it leans on.
//!
//! Split across three files (the multi-file `impl Evaluator { .. }` pattern):
//! this file holds `eval_expr` itself and the lang-block helpers;
//! [`crate::expr_binop`] holds unary/binary operator evaluation (including
//! the `&&`/`||` short-circuit chain); [`crate::expr_access`] holds
//! field/index/method-call access and the iterable-conversion helpers.

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
                // invoking it zero-arg in value position (defect #5, site/content/internals/language-conformance-contract.md).
                if let Some(v) = self.exec.env.get(name) {
                    Ok(v)
                } else if name == "now" {
                    // Relative anchor (site/content/internals/language-conformance-contract.md): live wall-clock datetime.
                    Ok(Value::DateTime(Box::new(crate::helpers::now_zoned(
                        self.host.clock.as_ref(),
                    ))))
                } else if name == "today" {
                    // Relative anchor (site/content/internals/language-conformance-contract.md): today at midnight.
                    Ok(Value::DateTime(Box::new(crate::helpers::today_zoned(
                        self.host.clock.as_ref(),
                    ))))
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
            Expr::Unary { op, expr, .. } => self.eval_unary(op, expr),
            Expr::Binary { .. } => self.eval_binary(expr, position),
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
                // Namespace constant access (`math.pi`, `config.<key>`): a
                // namespace name that isn't shadowed by a binding (site/content/internals/roadmap-and-priorities.md).
                if let Expr::Var { name: ns, .. } = &**recv
                    && self.exec.env.get(ns).is_none()
                    && crate::namespaces::is_namespace(ns)
                {
                    crate::namespaces::field(self, ns, name)
                } else {
                    let v = self.eval_expr(recv, Position::Value)?;
                    if *optional && v == Value::Null {
                        Ok(Value::Null)
                    } else {
                        self.field_or_method(v, name, span)
                    }
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
                    // Secret reads route through the SecretPort (site/content/internals/roadmap-and-priorities.md
                    // (site/content/internals/effects-plans-security.md). The default `StdSecret` resolves the same
                    // `SHOAL_SECRET_DIR`/`XDG_DATA_HOME`/`HOME` directory and
                    // opens the same `shoal_secret::SecretStore` as before.
                    let value = self
                        .host
                        .secrets
                        .get(secret_name)
                        .map_err(|e| ErrorVal::new("permission", e))?
                        .ok_or_else(|| {
                            ErrorVal::new("not_found", format!("secret `{secret_name}` not found"))
                        })?;
                    let text = String::from_utf8(value).map_err(|_| {
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
                // Namespace function call (`json.parse(s)`, `http.get(url)`,
                // `os.platform()`, `math.sqrt(2)`): a namespace name not shadowed
                // by a binding (site/content/internals/roadmap-and-priorities.md). Handled here (not methods.rs) because
                // several members reach the evaluator (session env, network, cwd).
                if let Expr::Var { name: ns, .. } = &**recv
                    && self.exec.env.get(ns).is_none()
                    && crate::namespaces::is_namespace(ns)
                {
                    let a = self.eval_args(args)?;
                    return crate::namespaces::call_method(self, ns, name, a)
                        .map_err(|e| e.or_span(span));
                }
                let v = self.eval_expr(recv, Position::Value)?;
                if *optional && v == Value::Null {
                    Ok(Value::Null)
                } else {
                    self.dispatch_method(v, name, args, span)
                }
            }
            Expr::FnCall { name, args, .. } => {
                // Structured builtins that take closures/thunks (site/content/internals/language-conformance-contract.md).
                match name.as_str() {
                    "parallel" => return self.builtin_parallel(args),
                    "retry" => return self.builtin_retry(args),
                    "on" => return self.builtin_on(args),
                    // Relative anchors as functions (site/content/internals/language-conformance-contract.md): `now()`/`today()`.
                    "now" if args.pos.is_empty() && args.named.is_empty() => {
                        return Ok(Value::DateTime(Box::new(crate::helpers::now_zoned(
                            self.host.clock.as_ref(),
                        ))));
                    }
                    "today" if args.pos.is_empty() && args.named.is_empty() => {
                        return Ok(Value::DateTime(Box::new(crate::helpers::today_zoned(
                            self.host.clock.as_ref(),
                        ))));
                    }
                    "assert" => {
                        let a = self.eval_args(args)?;
                        return self.builtin_assert(&a).map_err(|e| e.or_span(span));
                    }
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
                if let Some(f) = self.exec.env.get(name) {
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
                env: self.exec.env.clone(),
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
                    let old = self.exec.env.clone();
                    self.exec.env = old.child();
                    if let Some(p) = pattern {
                        self.bind_pattern(p, Value::Error(Arc::new(e)), false)?;
                    }
                    let r = self.block_value(handler);
                    self.exec.env = old;
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
                    let old = self.exec.env.clone();
                    self.exec.env = old.child();
                    if let Some(n) = binder {
                        self.exec
                            .env
                            .declare(n.clone(), Value::Error(Arc::new(e)), false);
                    }
                    let r = self.eval_expr(handler, Position::Value);
                    self.exec.env = old;
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

    /// Evaluate an interpreter block (site/content/internals/values-streams-execution.md): resolve `tool` as a command
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
}

/// Map an interpreter-class tool to how its `src` program reaches it (see
/// `site/content/internals/values-streams-execution.md`): the argv tail after
/// the resolved binary, plus `Some(bytes)`
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
