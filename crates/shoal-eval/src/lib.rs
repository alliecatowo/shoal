//! Tree-walk evaluator for shoal's canonical AST.

use shoal_ast::*;
use shoal_exec::{CancelToken, ExecMode, ExecSpec, StdinSpec};
use shoal_value::{
    CallArgs, CallCtx, ClosureVal, Env, ErrorVal, OutcomeVal, Record, VResult, Value,
};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Statement,
    Value,
}

pub struct Evaluator {
    pub env: Env,
    cwd: PathBuf,
    process_env: Vec<(OsString, OsString)>,
    pub interactive: bool,
    pub it: Value,
    cancel: CancelToken,
}

enum Flow {
    Value(Value),
    Return(Value),
    Break,
    Continue,
}

impl Evaluator {
    pub fn new(cwd: PathBuf) -> Self {
        Self {
            env: Env::root(),
            cwd,
            process_env: std::env::vars_os().collect(),
            interactive: false,
            it: Value::Null,
            cancel: CancelToken::new(),
        }
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Cancel the currently executing foreground process tree.
    pub fn cancel_current(&self) {
        self.cancel.cancel();
    }

    /// Install a fresh cancellation epoch before reading the next command.
    pub fn reset_cancel(&mut self) {
        self.cancel = CancelToken::new();
    }

    pub fn eval_program(&mut self, program: &Program) -> VResult<Value> {
        let mut last = Value::Null;
        for stmt in &program.stmts {
            match self.eval_stmt(stmt, true)? {
                Flow::Value(v) => {
                    last = v.clone();
                    self.it = v;
                }
                Flow::Return(_) => {
                    return Err(
                        ErrorVal::new("custom", "return outside function").with_span(stmt.span())
                    );
                }
                Flow::Break | Flow::Continue => {
                    return Err(
                        ErrorVal::new("custom", "loop control outside loop").with_span(stmt.span())
                    );
                }
            }
        }
        Ok(last)
    }

    fn eval_stmt(&mut self, stmt: &Stmt, top: bool) -> VResult<Flow> {
        match stmt {
            Stmt::Let {
                pattern,
                init,
                mutable,
                ..
            } => {
                let value = self.eval_expr(init, Position::Value)?;
                self.bind_pattern(pattern, value, *mutable)?;
                Ok(Flow::Value(Value::Null))
            }
            Stmt::Fn { decl } => {
                let closure = Value::Closure(Arc::new(ClosureVal {
                    name: Some(decl.name.clone()),
                    params: decl.params.clone(),
                    rest: decl.rest.clone(),
                    ret: decl.ret.clone(),
                    body: Expr::Block {
                        block: decl.body.clone(),
                        span: decl.body.span,
                    },
                    env: self.env.clone(),
                    doc: decl.doc.clone(),
                }));
                self.env.declare(decl.name.clone(), closure, false);
                Ok(Flow::Value(Value::Null))
            }
            Stmt::Alias { name, target, .. } => {
                self.env
                    .declare(name.clone(), Value::CmdRef(Arc::new(target.clone())), false);
                Ok(Flow::Value(Value::Null))
            }
            Stmt::Assign {
                target,
                op,
                value,
                span,
            } => {
                let rhs = self.eval_expr(value, Position::Value)?;
                let Expr::Var { name, .. } = target else {
                    return Err(ErrorVal::new(
                        "type_error",
                        "assignment target must be a variable in v0.1",
                    )
                    .with_span(*span));
                };
                let assigned = if *op == AssignOp::Set {
                    rhs
                } else {
                    let lhs = self.env.get(name).ok_or_else(|| {
                        ErrorVal::new("undefined_var", format!("undefined variable `{name}`"))
                    })?;
                    let bop = match op {
                        AssignOp::Add => BinOp::Add,
                        AssignOp::Sub => BinOp::Sub,
                        AssignOp::Mul => BinOp::Mul,
                        AssignOp::Div => BinOp::Div,
                        AssignOp::Set => unreachable!(),
                    };
                    shoal_value::ops::binop(bop, &lhs, &rhs)?
                };
                self.env.assign(name, assigned.clone()).map_err(|e| {
                    ErrorVal::new("type_error", format!("cannot assign `{name}`: {e:?}"))
                })?;
                Ok(Flow::Value(assigned))
            }
            Stmt::Expr { expr, .. } => Ok(Flow::Value(self.eval_expr(
                expr,
                if top {
                    Position::Statement
                } else {
                    Position::Value
                },
            )?)),
            Stmt::Return { value, .. } => Ok(Flow::Return(match value {
                Some(v) => self.eval_expr(v, Position::Value)?,
                None => Value::Null,
            })),
            Stmt::Break { .. } => Ok(Flow::Break),
            Stmt::Continue { .. } => Ok(Flow::Continue),
            Stmt::For {
                pattern,
                iter,
                body,
                ..
            } => {
                let iter_value = self.eval_expr(iter, Position::Value)?;
                let vals = self.into_values(iter_value)?;
                let mut last = Value::Null;
                for value in vals {
                    let old = self.env.clone();
                    self.env = old.child();
                    self.bind_pattern(pattern, value, false)?;
                    let flow = self.eval_block(body);
                    self.env = old;
                    match flow? {
                        Flow::Value(v) => last = v,
                        Flow::Continue => continue,
                        Flow::Break => break,
                        r @ Flow::Return(_) => return Ok(r),
                    }
                }
                Ok(Flow::Value(last))
            }
            Stmt::While { cond, body, .. } => {
                let mut last = Value::Null;
                while self.eval_expr(cond, Position::Value)?.as_condition()? {
                    match self.eval_block(body)? {
                        Flow::Value(v) => last = v,
                        Flow::Continue => {}
                        Flow::Break => break,
                        r @ Flow::Return(_) => return Ok(r),
                    }
                }
                Ok(Flow::Value(last))
            }
            Stmt::Use { span, .. } => Err(ErrorVal::new(
                "custom",
                "module loading is not implemented yet",
            )
            .with_span(*span)),
        }
    }

    fn eval_block(&mut self, block: &Block) -> VResult<Flow> {
        let old = self.env.clone();
        self.env = old.child();
        let mut last = Flow::Value(Value::Null);
        for stmt in &block.stmts {
            last = self.eval_stmt(stmt, false)?;
            if !matches!(last, Flow::Value(_)) {
                break;
            }
        }
        self.env = old;
        Ok(last)
    }

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
            Expr::DateTime { iso, .. } => iso
                .parse::<jiff::Zoned>()
                .map(|z| Value::DateTime(Box::new(z)))
                .map_err(|e| ErrorVal::new("arg_error", format!("invalid datetime: {e}"))),
            Expr::Var { name, .. } => self.env.get(name).ok_or_else(|| {
                ErrorVal::new("undefined_var", format!("undefined variable `{name}`"))
            }),
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
                op: BinOp::And,
                lhs,
                rhs,
                ..
            } => {
                let l = self.eval_expr(lhs, Position::Value)?.as_condition()?;
                if !l {
                    Ok(Value::Bool(false))
                } else {
                    Ok(Value::Bool(
                        self.eval_expr(rhs, Position::Value)?.as_condition()?,
                    ))
                }
            }
            Expr::Binary {
                op: BinOp::Or,
                lhs,
                rhs,
                ..
            } => {
                let l = self.eval_expr(lhs, Position::Value)?.as_condition()?;
                if l {
                    Ok(Value::Bool(true))
                } else {
                    Ok(Value::Bool(
                        self.eval_expr(rhs, Position::Value)?.as_condition()?,
                    ))
                }
            }
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
                let v = self.eval_expr(recv, Position::Value)?;
                if *optional && v == Value::Null {
                    Ok(Value::Null)
                } else {
                    let args = self.eval_args(args)?;
                    shoal_value::methods::call_method(self, v, name, args, span)
                }
            }
            Expr::FnCall { name, args, .. } => {
                let f = self.env.get(name).ok_or_else(|| {
                    ErrorVal::new("undefined_var", format!("undefined function `{name}`"))
                })?;
                let a = self.eval_args(args)?;
                self.call_value(&f, a)
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
            Expr::Block { block, .. } => match self.eval_block(block)? {
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
            Expr::ShRaw { src, .. } => self.run_argv(
                vec![
                    OsString::from("sh"),
                    OsString::from("-c"),
                    OsString::from(src),
                ],
                position,
                StdinSpec::Null,
                &[],
                span,
            ),
            Expr::With { cwd, env, body, .. } => {
                self.eval_with(cwd.as_deref(), env.as_deref(), body)
            }
            Expr::Spawn { body, .. } => self.spawn_block(body.clone()),
            Expr::Match {
                scrutinee, arms, ..
            } => self.eval_match(scrutinee, arms),
        };
        result.map_err(|e| e.or_span(span))
    }

    fn block_value(&mut self, b: &Block) -> VResult<Value> {
        match self.eval_block(b)? {
            Flow::Value(v) | Flow::Return(v) => Ok(v),
            Flow::Break | Flow::Continue => {
                Err(ErrorVal::new("custom", "loop control outside loop"))
            }
        }
    }

    fn eval_args(&mut self, args: &Args) -> VResult<CallArgs> {
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

    fn call_value(&mut self, f: &Value, args: CallArgs) -> VResult<Value> {
        match f {
            Value::Closure(c) => {
                let old = self.env.clone();
                self.env = c.env.child();
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
                            self.env = old;
                            return Err(ErrorVal::new(
                                "arg_error",
                                format!("missing argument `{}`", p.name),
                            ));
                        }
                    };
                    self.env.declare(p.name.clone(), val, false);
                }
                if let Some(rest) = &c.rest {
                    self.env.declare(
                        rest.name.clone(),
                        Value::List(args.pos.iter().skip(c.params.len()).cloned().collect()),
                        false,
                    );
                }
                let out = self.eval_expr(&c.body, Position::Value);
                self.env = old;
                out
            }
            Value::CmdRef(call) => {
                let mut call = (**call).clone();
                for v in args.pos {
                    call.args.push(self.value_cmd_arg(v, call.span)?);
                }
                self.eval_command(&call, Position::Value)
            }
            _ => Err(ErrorVal::new(
                "type_error",
                format!("{} is not callable", f.type_name()),
            )),
        }
    }

    fn eval_command(&mut self, call: &CmdCall, position: Position) -> VResult<Value> {
        if let Some(bound) = self.env.get(&call.head) {
            if !call.forced && bound.is_callable() {
                let mut pos = Vec::new();
                let mut named = Vec::new();
                for a in &call.args {
                    match a {
                        CmdArg::FlagLong { name, value, .. } => named.push((
                            name.clone(),
                            match value {
                                Some(v) => self.cmd_arg_value(v)?,
                                None => Value::Bool(true),
                            },
                        )),
                        _ => pos.extend(self.expand_arg(a)?),
                    }
                }
                return self.call_value(&bound, CallArgs { pos, named });
            }
        }
        if call.head == "cd" {
            let p = call
                .args
                .first()
                .map(|a| self.cmd_arg_value(a))
                .transpose()?
                .unwrap_or_else(|| {
                    Value::Path(std::env::home_dir().unwrap_or_else(|| PathBuf::from("/")))
                });
            let p = match p {
                Value::Path(p) => p,
                Value::Str(s) => PathBuf::from(s),
                _ => return Err(ErrorVal::new("arg_error", "cd expects path")),
            };
            self.cwd = if p.is_absolute() { p } else { self.cwd.join(p) }
                .canonicalize()
                .map_err(|e| ErrorVal::new("arg_error", e.to_string()))?;
            return Ok(Value::Path(self.cwd.clone()));
        }
        if call.head == "pwd" {
            return Ok(Value::Path(self.cwd.clone()));
        }
        let mut argv = vec![OsString::from(&call.head)];
        for a in &call.args {
            for v in self.expand_arg(a)? {
                argv.push(self.argv_value(v)?);
            }
        }
        let mut stdin = StdinSpec::Null;
        for r in &call.redirects {
            if r.kind == RedirectKind::In {
                stdin = StdinSpec::File(self.arg_path(&r.target)?);
            }
        }
        let value = self.run_argv(argv, position, stdin, &call.env_prefix, call.span)?;
        let Value::Outcome(out) = &value else {
            return Ok(value);
        };
        for r in &call.redirects {
            match r.kind {
                RedirectKind::Out => std::fs::write(self.arg_path(&r.target)?, &*out.stdout),
                RedirectKind::Append => {
                    use std::io::Write;
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(self.arg_path(&r.target)?)
                        .and_then(|mut f| f.write_all(&out.stdout))
                }
                RedirectKind::In => Ok(()),
            }
            .map_err(|e| ErrorVal::new("custom", e.to_string()))?;
        }
        Ok(value)
    }

    fn run_argv(
        &mut self,
        argv: Vec<OsString>,
        position: Position,
        stdin: StdinSpec,
        prefixes: &[EnvPrefix],
        span: Span,
    ) -> VResult<Value> {
        let mut env = self.process_env.clone();
        for p in prefixes {
            let v = self.cmd_arg_value(&p.value)?;
            let s = self.argv_value(v)?;
            if let Some(pair) = env.iter_mut().find(|x| x.0 == OsString::from(&p.name)) {
                pair.1 = s;
            } else {
                env.push((OsString::from(&p.name), s));
            }
        }
        let mode = if self.interactive && position == Position::Statement {
            ExecMode::PtyTee
        } else {
            ExecMode::Capture
        };
        let display = argv
            .iter()
            .map(|x| x.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        let r = shoal_exec::run(
            ExecSpec {
                argv,
                cwd: self.cwd.clone(),
                env,
                stdin,
                mode,
            },
            &self.cancel,
        )
        .map_err(|e| {
            ErrorVal::new(
                if e.kind() == std::io::ErrorKind::NotFound {
                    "not_found"
                } else {
                    "custom"
                },
                e.to_string(),
            )
            .with_span(span)
        })?;
        let ok = r.status == Some(0);
        let out = Value::Outcome(Arc::new(OutcomeVal {
            status: r.status,
            signal: r.signal,
            ok,
            stdout: Arc::new(r.stdout),
            stderr: Arc::new(r.stderr),
            dur_ns: r.dur.as_nanos().min(i64::MAX as u128) as i64,
            pid: r.pid,
            cmd: display,
        }));
        if !ok && position == Position::Statement {
            let Value::Outcome(failed) = &out else {
                unreachable!()
            };
            let message = match (failed.status, failed.signal.as_deref()) {
                (Some(code), _) => format!("`{}` exited with status {code}", failed.cmd),
                (_, Some(signal)) => format!("`{}` died from {signal}", failed.cmd),
                _ => format!("`{}` failed", failed.cmd),
            };
            Err(ErrorVal::new("cmd_failed", message)
                .with_status(failed.status)
                .with_stderr(String::from_utf8_lossy(&failed.stderr).into_owned()))
        } else {
            Ok(out)
        }
    }

    fn cmd_arg_value(&mut self, a: &CmdArg) -> VResult<Value> {
        match a {
            CmdArg::Word { text, .. } => Ok(Value::Str(text.clone())),
            CmdArg::Path { text, .. } => Ok(Value::Path(self.resolve_path(text))),
            CmdArg::Glob { pattern, .. } => Ok(Value::Glob(shoal_value::GlobVal {
                pattern: pattern.clone(),
                cwd: self.cwd.clone(),
                hidden: false,
            })),
            CmdArg::Str { expr, .. } | CmdArg::Expr { expr, .. } => {
                self.eval_expr(expr, Position::Value)
            }
            CmdArg::FlagLong { name, value, .. } => Ok(Value::Str(match value {
                Some(v) => format!(
                    "--{name}={}",
                    shoal_value::render::render_inline(&self.cmd_arg_value(v)?)
                ),
                None => format!("--{name}"),
            })),
            CmdArg::FlagShort { chars, .. } => Ok(Value::Str(format!("-{chars}"))),
            CmdArg::DashDash { .. } => Ok(Value::Str("--".into())),
            CmdArg::Dash { .. } => Ok(Value::Str("-".into())),
        }
    }
    fn expand_arg(&mut self, a: &CmdArg) -> VResult<Vec<Value>> {
        let v = self.cmd_arg_value(a)?;
        if let Value::Glob(g) = v {
            let pat = g.cwd.join(&g.pattern).to_string_lossy().into_owned();
            let mut paths = glob::glob(&pat)
                .map_err(|e| ErrorVal::new("arg_error", e.to_string()))?
                .filter_map(Result::ok)
                .map(Value::Path)
                .collect::<Vec<_>>();
            paths.sort_by_key(shoal_value::render::render_inline);
            Ok(paths)
        } else {
            Ok(vec![v])
        }
    }
    fn argv_value(&self, v: Value) -> VResult<OsString> {
        match v {
            Value::Str(s) => Ok(s.into()),
            Value::Path(p) => Ok(p.into_os_string()),
            Value::Int(i) => Ok(i.to_string().into()),
            Value::Float(f) => Ok(f.to_string().into()),
            Value::Size(n) => Ok(n.to_string().into()),
            Value::Duration(n) => Ok(n.to_string().into()),
            Value::Bool(b) => Ok(b.to_string().into()),
            Value::Secret(_) => Err(ErrorVal::new(
                "type_error",
                "secret cannot be placed in argv",
            )),
            other => Err(ErrorVal::new(
                "type_error",
                format!("{} cannot be passed as argv", other.type_name()),
            )),
        }
    }
    fn resolve_path(&self, text: &str) -> PathBuf {
        if let Some(rest) = text.strip_prefix("~/") {
            std::env::home_dir()
                .unwrap_or_else(|| self.cwd.clone())
                .join(rest)
        } else {
            PathBuf::from(text)
        }
    }
    fn arg_path(&mut self, a: &CmdArg) -> VResult<PathBuf> {
        match self.cmd_arg_value(a)? {
            Value::Path(p) => Ok(if p.is_absolute() { p } else { self.cwd.join(p) }),
            Value::Str(s) => {
                let p = PathBuf::from(s);
                Ok(if p.is_absolute() { p } else { self.cwd.join(p) })
            }
            _ => Err(ErrorVal::new("arg_error", "redirect target must be a path")),
        }
    }
    fn value_cmd_arg(&self, v: Value, span: Span) -> VResult<CmdArg> {
        Ok(match v {
            Value::Path(p) => CmdArg::Path {
                text: p.to_string_lossy().into_owned(),
                span,
            },
            Value::Str(s) => CmdArg::Word { text: s, span },
            _ => {
                return Err(ErrorVal::new(
                    "type_error",
                    "alias arguments must be strings or paths",
                ));
            }
        })
    }

    fn field(&self, v: Value, name: &str) -> VResult<Value> {
        match v {
            Value::Record(mut r) => r
                .shift_remove(name)
                .ok_or_else(|| ErrorVal::new("field_missing", format!("missing field `{name}`"))),
            Value::Outcome(o) => match name {
                "status" => Ok(o.status.map_or(Value::Null, |x| Value::Int(x as i64))),
                "ok" => Ok(Value::Bool(o.ok)),
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
                "pid" => Ok(Value::Int(o.pid as i64)),
                _ => Err(ErrorVal::new(
                    "field_missing",
                    format!("unknown outcome field `{name}`"),
                )),
            },
            _ => Err(ErrorVal::new(
                "field_missing",
                format!("{} has no field `{name}`", v.type_name()),
            )),
        }
    }
    fn index(&self, v: Value, idx: Value) -> VResult<Value> {
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
    fn into_values(&self, v: Value) -> VResult<Vec<Value>> {
        match v {
            Value::List(xs) => Ok(xs),
            Value::Table(rs) => Ok(rs.into_iter().map(Value::Record).collect()),
            Value::Range(r) => Ok(r.iter().map(Value::Int).collect()),
            Value::Stream(s) => s.take()?.collect(),
            _ => Err(ErrorVal::new("type_error", "value is not iterable")),
        }
    }

    fn bind_pattern(&mut self, p: &Pattern, v: Value, mutable: bool) -> VResult<()> {
        match p {
            Pattern::Wildcard { .. } => Ok(()),
            Pattern::Bind { name, .. } => {
                self.env.declare(name.clone(), v, mutable);
                Ok(())
            }
            Pattern::Lit { expr, .. } => {
                if self.eval_expr(expr, Position::Value)? == v {
                    Ok(())
                } else {
                    Err(ErrorVal::new("custom", "pattern did not match"))
                }
            }
            Pattern::List { items, rest, .. } => {
                let Value::List(xs) = v else {
                    return Err(ErrorVal::new("type_error", "list pattern requires list"));
                };
                if xs.len() < items.len() {
                    return Err(ErrorVal::new("custom", "list pattern did not match"));
                }
                for (p, v) in items.iter().zip(xs.iter().cloned()) {
                    self.bind_pattern(p, v, mutable)?;
                }
                if let Some(n) = rest {
                    self.env.declare(
                        n.clone(),
                        Value::List(xs.into_iter().skip(items.len()).collect()),
                        mutable,
                    );
                }
                Ok(())
            }
            _ => Err(ErrorVal::new(
                "custom",
                "this pattern form is not supported for binding yet",
            )),
        }
    }
    fn pattern_matches(&mut self, p: &Pattern, v: &Value) -> VResult<bool> {
        match p {
            Pattern::Wildcard { .. } => Ok(true),
            Pattern::Bind { name, .. } => {
                self.env.declare(name.clone(), v.clone(), false);
                Ok(true)
            }
            Pattern::Lit { expr, .. } => Ok(self.eval_expr(expr, Position::Value)? == *v),
            Pattern::Range {
                start,
                end,
                inclusive,
                ..
            } => {
                let a = self.eval_expr(start, Position::Value)?;
                let b = self.eval_expr(end, Position::Value)?;
                Ok(
                    shoal_value::ops::binop(BinOp::Ge, v, &a)? == Value::Bool(true)
                        && shoal_value::ops::binop(
                            if *inclusive { BinOp::Le } else { BinOp::Lt },
                            v,
                            &b,
                        )? == Value::Bool(true),
                )
            }
            _ => Ok(false),
        }
    }
    fn eval_match(&mut self, scrutinee: &Expr, arms: &[MatchArm]) -> VResult<Value> {
        let v = self.eval_expr(scrutinee, Position::Value)?;
        for arm in arms {
            let old = self.env.clone();
            self.env = old.child();
            let mut matched = false;
            for p in &arm.patterns {
                if self.pattern_matches(p, &v)? {
                    matched = true;
                    break;
                }
            }
            if matched
                && arm
                    .guard
                    .as_ref()
                    .map(|g| {
                        self.eval_expr(g, Position::Value)
                            .and_then(|x| x.as_condition())
                    })
                    .transpose()?
                    .unwrap_or(true)
            {
                let r = self.eval_expr(&arm.body, Position::Value);
                self.env = old;
                return r;
            }
            self.env = old;
        }
        Ok(Value::Null)
    }
    fn eval_with(
        &mut self,
        cwd: Option<&Expr>,
        env_expr: Option<&Expr>,
        body: &Block,
    ) -> VResult<Value> {
        let old_cwd = self.cwd.clone();
        let old_env = self.process_env.clone();
        if let Some(e) = cwd {
            match self.eval_expr(e, Position::Value)? {
                Value::Path(p) => self.cwd = if p.is_absolute() { p } else { self.cwd.join(p) },
                Value::Str(s) => self.cwd = self.cwd.join(s),
                _ => return Err(ErrorVal::new("type_error", "with cwd expects path")),
            }
        }
        if let Some(e) = env_expr {
            let Value::Record(r) = self.eval_expr(e, Position::Value)? else {
                return Err(ErrorVal::new("type_error", "with env expects record"));
            };
            for (k, v) in r {
                let val = self.argv_value(v)?;
                self.process_env.retain(|(n, _)| n != &OsString::from(&k));
                self.process_env.push((k.into(), val));
            }
        }
        let out = self.block_value(body);
        self.cwd = old_cwd;
        self.process_env = old_env;
        out
    }
    fn spawn_block(&mut self, body: Block) -> VResult<Value> {
        let task = shoal_value::TaskVal::new("spawn block");
        let worker = task.clone();
        let env = self.env.clone();
        let cwd = self.cwd.clone();
        let penv = self.process_env.clone();
        std::thread::spawn(move || {
            let mut ev = Evaluator::new(cwd);
            ev.env = env;
            ev.process_env = penv;
            worker.finish(ev.block_value(&body));
        });
        Ok(Value::Task(task))
    }
}

impl CallCtx for Evaluator {
    fn call_closure(&mut self, f: &Value, args: Vec<Value>) -> VResult<Value> {
        self.call_value(
            f,
            CallArgs {
                pos: args,
                named: vec![],
            },
        )
    }
    fn cwd(&self) -> PathBuf {
        self.cwd.clone()
    }
}

pub fn eval(program: &Program, cwd: impl AsRef<Path>) -> VResult<Value> {
    Evaluator::new(cwd.as_ref().to_path_buf()).eval_program(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> VResult<Value> {
        let program = shoal_syntax::parse(src).unwrap_or_else(|e| panic!("parse failed: {e}"));
        eval(&program, std::env::current_dir().unwrap())
    }

    #[test]
    fn arithmetic_and_binding() {
        assert_eq!(run("let x = 2 + 3\nx * 4").unwrap(), Value::Int(20));
    }

    #[test]
    fn strict_conditions_and_short_circuit() {
        assert_eq!(
            run("false && missing\ntrue || missing").unwrap(),
            Value::Bool(true)
        );
        assert_eq!(run("if true { 7 } else { 9 }").unwrap(), Value::Int(7));
        assert_eq!(run("if [1] { 2 }").unwrap_err().code, "type_error");
    }

    #[test]
    fn functions_are_callable() {
        assert_eq!(
            run("fn twice(x: int) { x * 2 }\ntwice(21)").unwrap(),
            Value::Int(42)
        );
    }

    #[test]
    fn captured_external_outcome_is_structured() {
        let value = run("let r = sh { printf hello }\nr.out").unwrap();
        assert_eq!(value, Value::Str("hello".into()));
    }

    #[test]
    fn failed_statement_preserves_process_diagnostics() {
        let err = run("sh { printf boom >&2; exit 7 }").unwrap_err();
        assert_eq!(err.code, "cmd_failed");
        assert_eq!(err.status, Some(7));
        assert_eq!(err.stderr.as_deref(), Some("boom"));
    }
}
