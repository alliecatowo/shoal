//! Tree-walk evaluator for shoal's canonical AST.

mod builtins;

use shoal_adapters::{AdapterCatalog, AdapterClass, SubSpec};
use shoal_ast::*;
use shoal_exec::{CancelToken, ExecMode, ExecSpec, StdinSpec};
use shoal_leash::{Effect, Estimates, Plan, Reversibility};
use shoal_value::{
    CallArgs, CallCtx, ClosureVal, Env, ErrorVal, OutcomeVal, Record, VResult, Value,
};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Position {
    Statement,
    Value,
}

/// Host renderer for statement-position outcomes (defect #1).
pub type StatementSink = Box<dyn FnMut(&Value) + Send>;

pub struct Evaluator {
    pub env: Env,
    cwd: PathBuf,
    process_env: Vec<(OsString, OsString)>,
    pub interactive: bool,
    pub it: Value,
    cancel: CancelToken,
    adapters: AdapterCatalog,
    /// Host renderer for statement-position outcomes (TDD §4.5, defect #1).
    sink: Option<StatementSink>,
    /// Runtime call-stack depth guard (defect #9).
    call_depth: usize,
    /// Nesting depth inside `fn` bodies — gates `cd`/env writes (defect #10).
    in_fn_body: usize,
    /// Live task registry backing the `jobs` builtin (defect #14).
    jobs: Vec<shoal_value::TaskVal>,
}

enum Flow {
    Value(Value),
    Return(Value),
    Break,
    Continue,
}

#[derive(Clone)]
struct ExecMeta {
    ok_codes: Vec<i32>,
    class: AdapterClass,
    parse: String,
    output_type: Option<String>,
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
            adapters: AdapterCatalog::empty(),
            sink: None,
            call_depth: 0,
            in_fn_body: 0,
            jobs: Vec::new(),
        }
    }

    /// Install the host's statement renderer (defect #1). Every statement-position
    /// command outcome (and every non-final top-level value) is routed here.
    /// When unset, a built-in default prints to real stdout so scripts behave
    /// without host wiring.
    pub fn set_statement_sink(&mut self, f: StatementSink) {
        self.sink = Some(f);
    }

    /// Bind `it` and append to the session `out` transcript list (REPL hook).
    /// `Var("it")` / `Var("out")` then resolve from the environment normally.
    pub fn record_transcript(&mut self, v: &Value) {
        self.env.declare("it", v.clone(), true);
        let mut out = match self.env.get("out") {
            Some(Value::List(xs)) => xs,
            _ => Vec::new(),
        };
        out.push(v.clone());
        self.env.declare("out", Value::List(out), true);
    }

    /// Route a value to the statement sink (or the default stdout renderer).
    fn emit(&mut self, v: &Value) {
        if let Some(sink) = self.sink.as_mut() {
            sink(v);
        } else {
            default_render(v);
        }
    }

    /// Route a statement value to the sink, skipping nulls and skipping
    /// interactive *external* outcomes (already streamed via PtyTee, defect #1).
    /// Builtin outcomes carry `pid == 0` and are never PtyTee-streamed, so they
    /// must still be rendered by the sink even interactively (outcome
    /// unification, REEF-cycle P1): only a real spawned child (`pid != 0`) was
    /// tee'd to the terminal and should be suppressed here.
    fn sink_value(&mut self, v: &Value) {
        if *v == Value::Null {
            return;
        }
        if self.interactive
            && let Value::Outcome(o) = v
            && o.pid != 0
        {
            return;
        }
        self.emit(v);
    }

    /// The task table backing the `jobs` builtin (defect #14).
    fn jobs_table(&self) -> Value {
        let rows = self
            .jobs
            .iter()
            .map(|t| {
                let mut r = Record::new();
                r.insert("id".into(), Value::Int(t.id as i64));
                r.insert("desc".into(), Value::Str(t.shared.desc.clone()));
                r.insert("done".into(), Value::Bool(t.is_done()));
                r
            })
            .collect();
        Value::Table(rows)
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn set_adapters(&mut self, adapters: AdapterCatalog) {
        self.adapters = adapters;
    }

    pub fn load_bundled_adapters(&mut self) -> Vec<String> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../adapters");
        let (catalog, warnings) = AdapterCatalog::load_dir(&root);
        self.adapters = catalog;
        warnings
    }

    /// Derive a conservative, concrete plan without spawning or mutating.
    pub fn plan_program(&mut self, program: &Program) -> VResult<Plan> {
        let mut effects = Vec::new();
        let mut functions = std::collections::HashMap::new();
        let mut aliases = std::collections::HashMap::new();
        for stmt in &program.stmts {
            if let Stmt::Fn { decl } = stmt {
                functions.insert(decl.name.clone(), decl.body.clone());
            }
            if let Stmt::Alias { name, target, .. } = stmt {
                aliases.insert(name.clone(), target.clone());
            }
        }
        for stmt in &program.stmts {
            self.plan_stmt(stmt, &functions, &aliases, &mut effects, 0)?;
        }
        let reversibility = if effects
            .iter()
            .any(|e| matches!(e, Effect::Opaque | Effect::FsDelete { .. }))
        {
            Reversibility::Unknown
        } else {
            Reversibility::Reversible
        };
        Ok(Plan::new(effects, reversibility, Estimates::default()))
    }

    /// Cancel the currently executing foreground process tree.
    pub fn cancel_current(&self) {
        self.cancel.cancel();
    }

    pub fn cancellation_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Install a fresh cancellation epoch before reading the next command.
    pub fn reset_cancel(&mut self) {
        self.cancel = CancelToken::new();
    }

    pub fn eval_program(&mut self, program: &Program) -> VResult<Value> {
        let mut last = Value::Null;
        let n = program.stmts.len();
        for (i, stmt) in program.stmts.iter().enumerate() {
            let is_last = i + 1 == n;
            match self.eval_stmt(stmt, true)? {
                Flow::Value(v) => {
                    self.it = v.clone();
                    if is_last {
                        last = v;
                    } else {
                        // Non-final statement values pass through to the sink
                        // (defect #1a); the final value is returned to the host.
                        self.sink_value(&v);
                    }
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
                // `env.NAME = v` — session environment write (defect #11, §4.6).
                if let Expr::Field { recv, name, .. } = target
                    && matches!(&**recv, Expr::Var { name, .. } if name == "env")
                {
                    if *op != AssignOp::Set {
                        return Err(ErrorVal::new(
                            "type_error",
                            "compound assignment is not allowed on env.NAME",
                        )
                        .with_span(*span));
                    }
                    if self.in_fn_body > 0 {
                        return Err(ErrorVal::new(
                            "custom",
                            "env writes are only allowed at session top level; use `with env:` inside a fn body",
                        )
                        .with_span(*span));
                    }
                    let val = self.argv_value(rhs.clone()).map_err(|e| e.or_span(*span))?;
                    self.process_env
                        .retain(|(k, _)| k != &OsString::from(name.clone()));
                    self.process_env.push((OsString::from(name.clone()), val));
                    return Ok(Flow::Value(rhs));
                }
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
            Stmt::Expr { expr, .. } => {
                let position = if top {
                    Position::Statement
                } else {
                    Position::Value
                };
                self.eval_expr_flow(expr, position)
            }
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
                let vals = self.values_from(iter_value)?;
                for value in vals {
                    let old = self.env.clone();
                    self.env = old.child();
                    self.bind_pattern(pattern, value, false)?;
                    let flow = self.eval_block(body, true);
                    self.env = old;
                    match flow? {
                        Flow::Value(_) => {}
                        Flow::Continue => continue,
                        Flow::Break => break,
                        r @ Flow::Return(_) => return Ok(r),
                    }
                }
                // A loop is a statement, not an expression — it yields no value
                // (so a trailing bare command in the body is not re-rendered as
                // the loop's result). Its work is its side effects.
                Ok(Flow::Value(Value::Null))
            }
            Stmt::While { cond, body, .. } => {
                while self.eval_expr(cond, Position::Value)?.as_condition()? {
                    match self.eval_block(body, true)? {
                        Flow::Value(_) => {}
                        Flow::Continue => {}
                        Flow::Break => break,
                        r @ Flow::Return(_) => return Ok(r),
                    }
                }
                Ok(Flow::Value(Value::Null))
            }
            Stmt::Use { span, .. } => Err(ErrorVal::new(
                "custom",
                "module loading is not implemented yet",
            )
            .with_span(*span)),
        }
    }

    /// Evaluate an expression appearing in statement position while letting
    /// `break`/`continue`/`return` inside an `if`/block body propagate to the
    /// enclosing loop rather than being flattened into a "loop control outside
    /// loop" error (the `while … { if … { break } }` case). Non-control-flow
    /// expressions fall back to ordinary value evaluation.
    fn eval_expr_flow(&mut self, expr: &Expr, position: Position) -> VResult<Flow> {
        match expr {
            Expr::If {
                cond,
                then,
                r#else,
                span,
            } => {
                let taken = self
                    .eval_expr(cond, Position::Value)?
                    .as_condition()
                    .map_err(|e| e.or_span(*span))?;
                if taken {
                    self.eval_block(then, false)
                } else if let Some(e) = r#else {
                    self.eval_expr_flow(e, position)
                } else {
                    Ok(Flow::Value(Value::Null))
                }
            }
            Expr::Block { block, .. } => self.eval_block(block, false),
            _ => Ok(Flow::Value(self.eval_expr(expr, position)?)),
        }
    }

    /// Evaluate a block. `sink_tail` says whether the caller will DISCARD the
    /// block's value (loop bodies) versus CONSUME it (fn body, `if`/block used
    /// as a value, or a top-level statement whose value `eval_program` itself
    /// sinks). The double-echo fix (P1): the trailing bare-command statement is
    /// the block VALUE and must NOT also be sunk here — only when the caller
    /// discards it (`sink_tail`) does its output route to the sink; otherwise
    /// the caller renders/sinks it exactly once. Non-final bare commands always
    /// print (they are intermediate, discard-context regardless).
    fn eval_block(&mut self, block: &Block, sink_tail: bool) -> VResult<Flow> {
        let old = self.env.clone();
        self.env = old.child();
        let mut last = Flow::Value(Value::Null);
        let n = block.stmts.len();
        for (i, stmt) in block.stmts.iter().enumerate() {
            let is_tail = i + 1 == n;
            // A statement is in discard context when it is not the block value,
            // or when the caller discards the block value.
            let discard = !is_tail || sink_tail;
            if let Stmt::Expr { expr, .. } = stmt
                && is_command_expr(expr)
            {
                // Discard-context commands run in statement position (failures
                // raise) and print; the value-context tail runs in value
                // position (failures surface as an outcome) and stays silent.
                let position = if discard {
                    Position::Statement
                } else {
                    Position::Value
                };
                let v = self.eval_expr(expr, position)?;
                if discard {
                    self.sink_value(&v);
                }
                last = Flow::Value(v);
                continue;
            }
            last = self.eval_stmt(stmt, false)?;
            if !matches!(last, Flow::Value(_)) {
                break;
            }
            // A discarded tail whose value is not a bare command (e.g. a nested
            // `if`/block that produced a command outcome) still routes to the
            // sink so loop-body side effects are not swallowed.
            if is_tail
                && sink_tail
                && let Flow::Value(v) = &last
            {
                self.sink_value(v);
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
            Expr::DateTime { iso, .. } => parse_datetime(iso).map(|z| Value::DateTime(Box::new(z))),
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
                    "run" => {
                        let mut a = self.eval_args(args)?;
                        if a.pos.is_empty() {
                            return Err(ErrorVal::arg_error(
                                "run expects a path or command name",
                            ));
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
                None,
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
        match self.eval_block(b, false)? {
            Flow::Value(v) | Flow::Return(v) => Ok(v),
            Flow::Break | Flow::Continue => {
                Err(ErrorVal::new("custom", "loop control outside loop"))
            }
        }
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
    fn eval_chain(&mut self, e: &Expr, emit: bool) -> VResult<Value> {
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
            if emit && is_command_expr(lhs) {
                self.sink_value(&l);
            }
            self.eval_chain(rhs, emit)
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
        // Runtime recursion guard (defect #9): unbounded native recursion aborts
        // the process, so cap the interpreter call stack well below that.
        self.call_depth += 1;
        if self.call_depth > 10_000 {
            self.call_depth -= 1;
            return Err(ErrorVal::new(
                "recursion_limit",
                "recursion limit exceeded (10000 nested calls)",
            ));
        }
        let r = self.call_value_inner(f, args);
        self.call_depth -= 1;
        r
    }

    fn call_value_inner(&mut self, f: &Value, args: CallArgs) -> VResult<Value> {
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
                // Track fn-body nesting so `cd`/env writes can be rejected (#10).
                self.in_fn_body += 1;
                let out = self.eval_expr(&c.body, Position::Value);
                self.in_fn_body -= 1;
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

    fn call_constructor(&self, name: &str, args: &CallArgs) -> VResult<Option<Value>> {
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
                        cwd: self.cwd.clone(),
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
            _ => Ok(None),
        }
    }

    fn eval_command(&mut self, call: &CmdCall, position: Position) -> VResult<Value> {
        // Session callables (fns/aliases) resolve as commands even when `^`-forced
        // (defect #3): `^` bypasses only non-callable let/var shadows.
        if let Some(bound) = self.env.get(&call.head)
            && bound.is_callable()
        {
            // `deploy --help` synthesises the signature + doc (§4.4, defect #12).
            if let Value::Closure(c) = &bound
                && call
                    .args
                    .iter()
                    .any(|a| matches!(a, CmdArg::FlagLong { name, .. } if name == "help"))
            {
                let help = closure_help(c);
                self.emit(&Value::Str(help));
                return Ok(Value::Null);
            }
            // A parameter typed `glob` owns expansion itself (TDD §4.3): the
            // callee receives the compiled, unexpanded pattern, so a glob-typed
            // positional slot must skip the generic glob-expansion path below.
            let closure_sig: Option<(&[Param], Option<&RestParam>)> = match &bound {
                Value::Closure(c) => Some((&c.params, c.rest.as_ref())),
                _ => None,
            };
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
                    CmdArg::Glob { .. }
                        if closure_sig.is_some_and(|(params, rest)| {
                            expected_param_ty(params, rest, pos.len()) == Some("glob")
                        }) =>
                    {
                        pos.push(self.cmd_arg_value(a)?);
                    }
                    _ => pos.extend(self.expand_arg(a)?),
                }
            }
            // Coerce CMD words to the callee's declared param types (defect #12).
            if let Value::Closure(c) = &bound {
                coerce_call_args(&c.params, c.rest.as_ref(), &mut pos, &mut named)?;
            }
            return self.call_value(&bound, CallArgs { pos, named });
        }
        // A bare word bound to a non-callable value (e.g. `it`, `out`, or any
        // `let`) resolves to that value — bound names dispatch as EXPR (§3.1.3).
        if let Some(bound) = self.env.get(&call.head)
            && !call.forced
            && !bound.is_callable()
            && call.args.is_empty()
            && call.redirects.is_empty()
            && call.env_prefix.is_empty()
        {
            return Ok(bound);
        }
        if call.head == "jobs" {
            return Ok(self.jobs_table());
        }
        if call.head == "interact" {
            return self.builtin_interact(call);
        }
        if call.head == "open" {
            let vs = self.collect_cmd_values(call)?;
            return self.builtin_open(vs);
        }
        if call.head == "save" {
            let vs = self.collect_cmd_values(call)?;
            return self.builtin_save(vs);
        }
        if builtins::is_builtin(&call.head) {
            // Outcome unification (P1a): a builtin yields a `Value::Outcome`
            // exactly like an external command — its structured result becomes
            // the outcome's `.out` (`parsed`), `status = 0`/`ok = true`. A
            // builtin error still raises as before (via `?`).
            let value = builtins::run(self, call)?;
            let outcome = builtin_outcome(&call.head, value);
            // Redirects apply to builtin results too (defect #8).
            return self.apply_builtin_redirects(call, outcome);
        }
        if call.head == "cd" {
            if self.in_fn_body > 0 {
                return Err(ErrorVal::new(
                    "custom",
                    "cd is only allowed at session top level; use `with cwd:` inside a fn body",
                )
                .with_span(call.span));
            }
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
        // `run` is the poly runner + dynamic form (pty §8): dispatch by extension
        // or, for a non-path name, invoke dynamically as a command.
        if call.head == "run" {
            let mut vs = self.collect_cmd_values(call)?;
            if vs.is_empty() {
                return Err(ErrorVal::arg_error("run expects a path or command name"));
            }
            let target = vs.remove(0);
            return self.run_poly(target, vs, position);
        }
        if call.head == "source" || call.head.ends_with(".shl") {
            let is_source = call.head == "source";
            let script_path = if is_source {
                let p = call
                    .args
                    .first()
                    .map(|a| self.cmd_arg_value(a))
                    .transpose()?
                    .ok_or_else(|| ErrorVal::new("arg_error", "source expects script path"))?;
                match p {
                    Value::Path(p) => p,
                    Value::Str(s) => PathBuf::from(s),
                    _ => return Err(ErrorVal::new("arg_error", "expects path")),
                }
            } else {
                PathBuf::from(&call.head)
            };

            let path = if script_path.is_absolute() {
                script_path
            } else {
                self.cwd.join(script_path)
            };
            let src = std::fs::read_to_string(&path)
                .map_err(|e| ErrorVal::new("io_error", format!("cannot read script: {e}")))?;
            let program = shoal_syntax::parse(&src)
                .map_err(|e| ErrorVal::new("parse_error", e.to_string()))?;

            if is_source {
                return self.eval_program(&program);
            } else {
                let mut child = Evaluator::new(self.cwd.clone());
                child.env = self.env.clone();
                child.process_env = self.process_env.clone();
                child.adapters = self.adapters.clone();
                return child.eval_program(&program);
            }
        }
        if self.adapters.lookup(&call.head).is_some() {
            return self.eval_adapter(call, position);
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
        let value = self.run_argv(argv, position, stdin, &call.env_prefix, call.span, None)?;
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

    fn eval_adapter(&mut self, call: &CmdCall, position: Position) -> VResult<Value> {
        let adapter = self
            .adapters
            .lookup(&call.head)
            .expect("checked adapter")
            .clone();
        let (spec, sub, start) = match call.args.first() {
            Some(CmdArg::Word { text, .. }) if adapter.subs.contains_key(text) => {
                (adapter.subs[text].clone(), Some(text.clone()), 1)
            }
            _ => (adapter.top.clone(), None, 0),
        };
        let mut argv = vec![OsString::from(&adapter.bin)];
        match (&spec.invoke, &sub) {
            (Some(rewrite), _) => argv.extend(rewrite.iter().map(OsString::from)),
            (None, Some(sub)) => argv.push(sub.into()),
            (None, None) => {}
        }
        let mut positional = 0usize;
        let mut i = start;
        while i < call.args.len() {
            match &call.args[i] {
                CmdArg::FlagLong { name, value, .. } => {
                    let param = spec
                        .params
                        .iter()
                        .find(|p| p.name == *name)
                        .ok_or_else(|| {
                            ErrorVal::arg_error(format!(
                                "{}: unknown flag --{name}; expected {}",
                                call.head,
                                signature(&spec)
                            ))
                        })?;
                    // `consumed` flags stay recognized/validated (below) but
                    // must never reach the child's argv — see the module-level
                    // "consumed" rule doc in shoal-adapters.
                    let consumed = spec.consumed.iter().any(|c| c == name);
                    if !consumed {
                        argv.push(format!("--{}", name.replace('_', "-")).into());
                    }
                    if let Some(value) = value {
                        let v = self.cmd_arg_value(value)?;
                        validate_adapter_value(&v, &param.ty)?;
                        if !consumed {
                            argv.push(self.argv_value(v)?);
                        }
                    } else if !param.ty.trim_end_matches('?').eq("bool") {
                        i += 1;
                        let next = call.args.get(i).ok_or_else(|| {
                            ErrorVal::arg_error(format!("--{name} requires a value"))
                        })?;
                        let v = self.cmd_arg_value(next)?;
                        validate_adapter_value(&v, &param.ty)?;
                        if !consumed {
                            argv.push(self.argv_value(v)?);
                        }
                    }
                }
                CmdArg::FlagShort { chars, .. } => {
                    let mut kept = String::new();
                    for ch in chars.chars() {
                        let Some(pname) = spec.short_flags.get(&ch.to_string()) else {
                            return Err(ErrorVal::arg_error(format!(
                                "{}: unknown short flag -{ch}",
                                call.head
                            )));
                        };
                        // Same "consumed" rule as the long-flag branch above:
                        // stays a recognized short flag, just dropped from argv.
                        if !spec.consumed.iter().any(|c| c == pname) {
                            kept.push(ch);
                        }
                    }
                    if !kept.is_empty() {
                        argv.push(format!("-{kept}").into());
                    }
                }
                CmdArg::DashDash { .. } => argv.push("--".into()),
                arg => {
                    let expected = spec
                        .positional
                        .get(positional)
                        .and_then(|name| spec.params.iter().find(|p| &p.name == name));
                    let value = self.cmd_arg_value(arg)?;
                    if let Some(param) = expected {
                        validate_adapter_value(&value, &param.ty)?;
                    }
                    // A parameter typed glob owns expansion; T0/list<path> expansion remains elsewhere.
                    if matches!(expected.map(|p| p.ty.trim_end_matches('?')), Some("glob")) {
                        match value {
                            Value::Glob(g) => argv.push(g.pattern.into()),
                            v => argv.push(self.argv_value(v)?),
                        }
                    } else if matches!(value, Value::Glob(_)) {
                        for value in self.expand_arg(arg)? {
                            argv.push(self.argv_value(value)?);
                        }
                    } else {
                        argv.push(self.argv_value(value)?);
                    }
                    positional += 1;
                }
            }
            i += 1;
        }
        let ok_codes = spec.ok_codes.clone().unwrap_or(adapter.ok_codes);
        let meta = ExecMeta {
            ok_codes,
            class: adapter.class,
            parse: spec.parse,
            output_type: spec.output_type,
        };
        self.run_argv(
            argv,
            position,
            StdinSpec::Null,
            &call.env_prefix,
            call.span,
            Some(meta),
        )
    }

    fn plan_stmt(
        &mut self,
        stmt: &Stmt,
        functions: &std::collections::HashMap<String, Block>,
        aliases: &std::collections::HashMap<String, CmdCall>,
        out: &mut Vec<Effect>,
        depth: usize,
    ) -> VResult<()> {
        match stmt {
            Stmt::Expr { expr, .. } => self.plan_expr(expr, functions, aliases, out, depth),
            Stmt::Let { init, .. } | Stmt::Assign { value: init, .. } => {
                self.plan_expr(init, functions, aliases, out, depth)
            }
            Stmt::Return {
                value: Some(expr), ..
            } => self.plan_expr(expr, functions, aliases, out, depth),
            Stmt::For { iter, body, .. } => {
                self.plan_expr(iter, functions, aliases, out, depth)?;
                self.plan_block(body, functions, aliases, out, depth)
            }
            Stmt::While { cond, body, .. } => {
                self.plan_expr(cond, functions, aliases, out, depth)?;
                self.plan_block(body, functions, aliases, out, depth)
            }
            _ => Ok(()),
        }
    }

    fn plan_block(
        &mut self,
        block: &Block,
        functions: &std::collections::HashMap<String, Block>,
        aliases: &std::collections::HashMap<String, CmdCall>,
        out: &mut Vec<Effect>,
        depth: usize,
    ) -> VResult<()> {
        for stmt in &block.stmts {
            self.plan_stmt(stmt, functions, aliases, out, depth)?;
        }
        Ok(())
    }

    fn plan_expr(
        &mut self,
        expr: &Expr,
        functions: &std::collections::HashMap<String, Block>,
        aliases: &std::collections::HashMap<String, CmdCall>,
        out: &mut Vec<Effect>,
        depth: usize,
    ) -> VResult<()> {
        match expr {
            Expr::Cmd { call, .. } => self.plan_call(call, functions, aliases, out, depth),
            Expr::ShRaw { .. } => {
                push_effect(out, Effect::Opaque);
                Ok(())
            }
            Expr::Block { block, .. } | Expr::Spawn { body: block, .. } => {
                self.plan_block(block, functions, aliases, out, depth)
            }
            Expr::If {
                cond, then, r#else, ..
            } => {
                self.plan_expr(cond, functions, aliases, out, depth)?;
                self.plan_block(then, functions, aliases, out, depth)?;
                if let Some(other) = r#else {
                    self.plan_expr(other, functions, aliases, out, depth)?;
                }
                Ok(())
            }
            Expr::Try { body, handler, .. } => {
                self.plan_block(body, functions, aliases, out, depth)?;
                self.plan_block(handler, functions, aliases, out, depth)
            }
            Expr::Catch { expr, handler, .. } => {
                self.plan_expr(expr, functions, aliases, out, depth)?;
                self.plan_expr(handler, functions, aliases, out, depth)
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.plan_expr(lhs, functions, aliases, out, depth)?;
                self.plan_expr(rhs, functions, aliases, out, depth)
            }
            Expr::Unary { expr, .. } | Expr::Field { recv: expr, .. } => {
                self.plan_expr(expr, functions, aliases, out, depth)
            }
            Expr::Index { recv, index, .. } => {
                self.plan_expr(recv, functions, aliases, out, depth)?;
                self.plan_expr(index, functions, aliases, out, depth)
            }
            Expr::MethodCall { recv, args, .. } => {
                self.plan_expr(recv, functions, aliases, out, depth)?;
                for e in &args.pos {
                    self.plan_expr(e, functions, aliases, out, depth)?;
                }
                for n in &args.named {
                    self.plan_expr(&n.value, functions, aliases, out, depth)?;
                }
                Ok(())
            }
            Expr::FnCall { name, args, .. } => {
                for e in &args.pos {
                    self.plan_expr(e, functions, aliases, out, depth)?;
                }
                for n in &args.named {
                    self.plan_expr(&n.value, functions, aliases, out, depth)?;
                }
                if let Some(body) = functions.get(name) {
                    self.plan_block(body, functions, aliases, out, depth + 1)?;
                }
                Ok(())
            }
            Expr::List { items, .. } => {
                for e in items {
                    self.plan_expr(e, functions, aliases, out, depth)?;
                }
                Ok(())
            }
            Expr::Record { fields, .. } => {
                for f in fields {
                    self.plan_expr(&f.value, functions, aliases, out, depth)?;
                }
                Ok(())
            }
            Expr::Range { start, end, .. } => {
                self.plan_expr(start, functions, aliases, out, depth)?;
                self.plan_expr(end, functions, aliases, out, depth)
            }
            Expr::With { cwd, env, body, .. } => {
                if let Some(e) = cwd {
                    self.plan_expr(e, functions, aliases, out, depth)?
                }
                if let Some(e) = env {
                    self.plan_expr(e, functions, aliases, out, depth)?
                }
                self.plan_block(body, functions, aliases, out, depth)
            }
            Expr::Match {
                scrutinee, arms, ..
            } => {
                self.plan_expr(scrutinee, functions, aliases, out, depth)?;
                for arm in arms {
                    if let Some(g) = &arm.guard {
                        self.plan_expr(g, functions, aliases, out, depth)?
                    }
                    self.plan_expr(&arm.body, functions, aliases, out, depth)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn plan_call(
        &mut self,
        call: &CmdCall,
        functions: &std::collections::HashMap<String, Block>,
        aliases: &std::collections::HashMap<String, CmdCall>,
        out: &mut Vec<Effect>,
        depth: usize,
    ) -> VResult<()> {
        if depth > 64 {
            return Err(ErrorVal::new(
                "recursion_limit",
                "planning function recursion exceeded 64",
            ));
        }
        if let Some(target) = aliases.get(&call.head) {
            return self.plan_call(target, functions, aliases, out, depth + 1);
        }
        if let Some(body) = functions.get(&call.head) {
            return self.plan_block(body, functions, aliases, out, depth + 1);
        }
        if builtins::is_builtin(&call.head) || matches!(call.head.as_str(), "cd" | "pwd") {
            for effect in self.builtin_effects(call)? {
                push_effect(out, effect);
            }
            return Ok(());
        }
        if let Some(adapter) = self.adapters.lookup(&call.head).cloned() {
            let (spec, start) = match call.args.first() {
                Some(CmdArg::Word { text, .. }) if adapter.subs.contains_key(text) => {
                    (adapter.subs[text].clone(), 1)
                }
                _ => (adapter.top.clone(), 0),
            };
            let bindings = self.plan_bindings(call, &spec, start)?;
            for declared in &spec.effects {
                for effect in parse_declared_effect(declared, &bindings, &self.cwd) {
                    push_effect(out, effect);
                }
            }
            push_effect(
                out,
                Effect::ProcSpawn {
                    bin_hash: String::new(),
                    argv0: adapter.bin,
                },
            );
        } else {
            push_effect(out, Effect::Opaque);
        }
        Ok(())
    }

    fn builtin_effects(&self, call: &CmdCall) -> VResult<Vec<Effect>> {
        let mut ps = Vec::new();
        for arg in &call.args {
            if !matches!(
                arg,
                CmdArg::FlagLong { .. } | CmdArg::FlagShort { .. } | CmdArg::DashDash { .. }
            ) {
                ps.extend(plan_paths(arg, &self.cwd)?);
            }
        }
        let e = match call.head.as_str() {
            "echo" | "sleep" | "pwd" => vec![],
            "env" => vec![Effect::EnvRead {
                names: vec!["*".into()],
            }],
            "which" => vec![Effect::EnvRead {
                names: vec!["PATH".into()],
            }],
            "ls" | "cat" | "stat" => vec![Effect::FsRead {
                paths: if ps.is_empty() {
                    vec![self.cwd.clone()]
                } else {
                    ps
                },
            }],
            "mkdir" | "touch" => vec![Effect::FsWrite { paths: ps }],
            "cp" => {
                if ps.len() < 2 {
                    return Err(ErrorVal::arg_error("cp requires source and destination"));
                }
                let dst = ps.last().cloned().unwrap();
                vec![
                    Effect::FsRead {
                        paths: ps[..ps.len() - 1].to_vec(),
                    },
                    Effect::FsWrite { paths: vec![dst] },
                ]
            }
            "mv" => {
                if ps.len() < 2 {
                    return Err(ErrorVal::arg_error("mv requires source and destination"));
                }
                let dst = ps.last().cloned().unwrap();
                vec![
                    Effect::FsRead {
                        paths: ps[..ps.len() - 1].to_vec(),
                    },
                    Effect::FsWrite { paths: vec![dst] },
                    Effect::FsDelete {
                        paths: ps[..ps.len() - 1].to_vec(),
                    },
                ]
            }
            "rm" => vec![Effect::FsDelete { paths: ps }],
            "cd" => vec![Effect::SessionWrite],
            _ => vec![],
        };
        Ok(e)
    }

    fn plan_bindings(
        &self,
        call: &CmdCall,
        spec: &SubSpec,
        start: usize,
    ) -> VResult<std::collections::HashMap<String, Vec<String>>> {
        let mut bindings = std::collections::HashMap::new();
        let mut positional = 0;
        for arg in &call.args[start..] {
            match arg {
                CmdArg::FlagLong { name, value, .. } => {
                    if let Some(value) = value {
                        bindings
                            .entry(name.clone())
                            .or_insert_with(Vec::new)
                            .push(plan_text(value)?);
                    }
                }
                CmdArg::FlagShort { .. } | CmdArg::DashDash { .. } => {}
                arg => {
                    if let Some(name) = spec.positional.get(positional) {
                        bindings
                            .entry(name.clone())
                            .or_insert_with(Vec::new)
                            .push(plan_text(arg)?);
                    }
                    positional += 1;
                }
            }
        }
        Ok(bindings)
    }

    fn run_argv(
        &mut self,
        argv: Vec<OsString>,
        position: Position,
        stdin: StdinSpec,
        prefixes: &[EnvPrefix],
        span: Span,
        meta: Option<ExecMeta>,
    ) -> VResult<Value> {
        let mut env = self.process_env.clone();
        for p in prefixes {
            let v = self.cmd_arg_value(&p.value)?;
            let s = match v {
                Value::Secret(secret) => OsString::from(secret.value.as_ref()),
                other => self.argv_value(other)?,
            };
            if let Some(pair) = env.iter_mut().find(|x| x.0 == OsString::from(&p.name)) {
                pair.1 = s;
            } else {
                env.push((OsString::from(&p.name), s));
            }
        }
        let force_tui = meta.as_ref().is_some_and(|m| m.class == AdapterClass::Tui);
        let mode = if force_tui || (self.interactive && position == Position::Statement) {
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
                sandbox: None,
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
        let ok_codes = meta.as_ref().map_or(&[0][..], |m| m.ok_codes.as_slice());
        let ok = r.status.is_some_and(|code| ok_codes.contains(&code));
        let parsed = meta.as_ref().and_then(|m| {
            shoal_adapters::parse_output(&m.parse, &r.stdout, m.output_type.as_deref())
        });
        let out = Value::Outcome(Arc::new(OutcomeVal {
            status: r.status,
            signal: r.signal,
            ok,
            stdout: Arc::new(r.stdout),
            stderr: Arc::new(r.stderr),
            dur_ns: r.dur.as_nanos().min(i64::MAX as u128) as i64,
            pid: r.pid,
            cmd: display,
            parsed,
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
            // Zero-match glob lint (defect #16, §1.5): nullglob still yields zero
            // argv, but a statement-level miss is worth a diagnostic.
            if paths.is_empty() {
                eprintln!("shoal: no matches for {}", g.pattern);
            }
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
    fn values_from(&self, v: Value) -> VResult<Vec<Value>> {
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
            // `int n` / `str s` — runtime type test, bind on success (TDD §3.2).
            Pattern::Type { ty, name, .. } => {
                if v.type_name() == ty.name {
                    if let Some(n) = name {
                        self.env.declare(n.clone(), v.clone(), false);
                    }
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            // `{ field, field: subpat }` — open record match: scrutinee must be
            // a record containing every named field; extra fields are ignored.
            Pattern::Record { fields, .. } => {
                let Value::Record(map) = v else {
                    return Ok(false);
                };
                for f in fields {
                    let Some(fv) = map.get(&f.name) else {
                        return Ok(false);
                    };
                    let fv = fv.clone();
                    match &f.pattern {
                        Some(sub) => {
                            if !self.pattern_matches(sub, &fv)? {
                                return Ok(false);
                            }
                        }
                        None => self.env.declare(f.name.clone(), fv, false),
                    }
                }
                Ok(true)
            }
            // `[a, b, ...rest]` — shape match over a list.
            Pattern::List { items, rest, .. } => {
                let Value::List(xs) = v else {
                    return Ok(false);
                };
                if rest.is_some() {
                    if xs.len() < items.len() {
                        return Ok(false);
                    }
                } else if xs.len() != items.len() {
                    return Ok(false);
                }
                for (p, ev) in items.iter().zip(xs.iter()) {
                    let ev = ev.clone();
                    if !self.pattern_matches(p, &ev)? {
                        return Ok(false);
                    }
                }
                if let Some(r) = rest {
                    self.env
                        .declare(r.clone(), Value::List(xs[items.len()..].to_vec()), false);
                }
                Ok(true)
            }
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
        // Structured cancellation: cancelling the task cancels the child's exec
        // tokens (defect #14).
        let child_cancel = CancelToken::new();
        let hook_cancel = child_cancel.clone();
        task.on_cancel(Box::new(move || hook_cancel.cancel()));
        let worker = task.clone();
        let env = self.env.clone();
        let cwd = self.cwd.clone();
        let penv = self.process_env.clone();
        let adapters = self.adapters.clone();
        std::thread::spawn(move || {
            let mut ev = Evaluator::new(cwd);
            ev.env = env;
            ev.process_env = penv;
            ev.adapters = adapters;
            ev.cancel = child_cancel;
            worker.finish(ev.block_value(&body));
        });
        self.jobs.push(task.clone());
        Ok(Value::Task(task))
    }

    /// True when `name` resolves as a command (builtin, special head, adapter,
    /// or an executable on `PATH`) — drives command-in-expression (defect #5).
    fn is_command_name(&self, name: &str) -> bool {
        if builtins::is_builtin(name)
            || matches!(
                name,
                "cd" | "pwd" | "source" | "run" | "jobs" | "interact" | "open" | "save"
            )
        {
            return true;
        }
        if name.contains('/') || name.contains('.') {
            return false;
        }
        if self.adapters.lookup(name).is_some() {
            return true;
        }
        let path = self
            .process_env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.as_os_str());
        shoal_exec::which(OsStr::new(name), path).is_some()
    }

    /// Collect a command's positional (non-flag) argument values.
    fn collect_cmd_values(&mut self, call: &CmdCall) -> VResult<Vec<Value>> {
        let mut vs = Vec::new();
        for a in &call.args {
            match a {
                CmdArg::FlagLong { .. } | CmdArg::FlagShort { .. } | CmdArg::DashDash { .. } => {}
                _ => vs.extend(self.expand_arg(a)?),
            }
        }
        Ok(vs)
    }

    fn apply_builtin_redirects(&mut self, call: &CmdCall, value: Value) -> VResult<Value> {
        let mut captured = false;
        for r in &call.redirects {
            match r.kind {
                RedirectKind::Out => {
                    let p = self.arg_path(&r.target)?;
                    std::fs::write(&p, value_bytes(&value))
                        .map_err(|e| ErrorVal::new("custom", e.to_string()))?;
                    captured = true;
                }
                RedirectKind::Append => {
                    use std::io::Write;
                    let p = self.arg_path(&r.target)?;
                    std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&p)
                        .and_then(|mut f| f.write_all(&value_bytes(&value)))
                        .map_err(|e| ErrorVal::new("custom", e.to_string()))?;
                    captured = true;
                }
                RedirectKind::In => {}
            }
        }
        // `cmd > file` / `>> file` sends the output to the file — it must not
        // also be rendered to the statement sink (defect #8). Yield Null so the
        // redirected statement stays silent on stdout.
        if captured {
            Ok(Value::Null)
        } else {
            Ok(value)
        }
    }

    /// Force a real PTY for `interact <cmd…>` (§5).
    fn builtin_interact(&mut self, call: &CmdCall) -> VResult<Value> {
        let vs = self.collect_cmd_values(call)?;
        if vs.is_empty() {
            return Err(ErrorVal::arg_error("interact expects a command"));
        }
        let mut argv = Vec::new();
        for v in vs {
            argv.push(self.argv_value(v)?);
        }
        let saved = self.interactive;
        self.interactive = true;
        let r = self.run_argv(
            argv,
            Position::Statement,
            StdinSpec::Inherit,
            &[],
            call.span,
            None,
        );
        self.interactive = saved;
        r
    }

    /// `open <path>` — detached `xdg-open` (§5).
    fn builtin_open(&mut self, pos: Vec<Value>) -> VResult<Value> {
        if pos.len() != 1 {
            return Err(ErrorVal::arg_error("open expects exactly one path"));
        }
        let p = match &pos[0] {
            Value::Path(p) => p.clone(),
            Value::Str(s) => PathBuf::from(s),
            v => {
                return Err(ErrorVal::type_error(format!(
                    "open expects a path, found {}",
                    v.type_name()
                )));
            }
        };
        let p = if p.is_absolute() { p } else { self.cwd.join(p) };
        std::process::Command::new("xdg-open")
            .arg(&p)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| ErrorVal::new("custom", format!("open: {e}")))?;
        Ok(Value::Null)
    }

    /// `save(path, value)` builtin form (§5) — delegates to the value method.
    fn builtin_save(&mut self, pos: Vec<Value>) -> VResult<Value> {
        if pos.len() != 2 {
            return Err(ErrorVal::arg_error("save expects (path, value)"));
        }
        let path = pos[0].clone();
        let value = pos[1].clone();
        shoal_value::methods::call_method(
            self,
            value,
            "save",
            CallArgs {
                pos: vec![path],
                named: vec![],
            },
            Span::default(),
        )
    }

    /// `parallel(...closures)` — fail-fast by default; `settle: true` collects all
    /// outcomes (§5).
    fn builtin_parallel(&mut self, args: &Args) -> VResult<Value> {
        let a = self.eval_args(args)?;
        let settle = a
            .named
            .iter()
            .find(|(n, _)| n == "settle")
            .map(|(_, v)| matches!(v, Value::Bool(true)))
            .unwrap_or(false);
        let mut handles = Vec::new();
        for f in a.pos {
            let env = self.env.clone();
            let cwd = self.cwd.clone();
            let penv = self.process_env.clone();
            let adapters = self.adapters.clone();
            handles.push(std::thread::spawn(move || {
                let mut ev = Evaluator::new(cwd);
                ev.env = env;
                ev.process_env = penv;
                ev.adapters = adapters;
                ev.call_value(&f, CallArgs::default())
            }));
        }
        let mut results = Vec::new();
        let mut first_err: Option<ErrorVal> = None;
        for h in handles {
            match h.join() {
                Ok(Ok(v)) => results.push(v),
                Ok(Err(e)) => {
                    first_err.get_or_insert_with(|| e.clone());
                    results.push(Value::Error(Arc::new(e)));
                }
                Err(_) => {
                    let e = ErrorVal::new("custom", "parallel task panicked");
                    first_err.get_or_insert_with(|| e.clone());
                    results.push(Value::Error(Arc::new(e)));
                }
            }
        }
        if let Some(e) = first_err
            && !settle
        {
            return Err(e);
        }
        Ok(Value::List(results))
    }

    /// `retry(n, thunk, delay: duration?)` — retry a thunk until it succeeds (§5).
    fn builtin_retry(&mut self, args: &Args) -> VResult<Value> {
        let a = self.eval_args(args)?;
        let n = match a.pos.first() {
            Some(Value::Int(i)) if *i > 0 => *i as usize,
            _ => return Err(ErrorVal::arg_error("retry expects a positive attempt count")),
        };
        let thunk = a
            .pos
            .get(1)
            .cloned()
            .ok_or_else(|| ErrorVal::arg_error("retry expects a thunk"))?;
        let delay = a.named.iter().find(|(k, _)| k == "delay").and_then(|(_, v)| {
            if let Value::Duration(ns) = v {
                Some(*ns)
            } else {
                None
            }
        });
        let mut last = ErrorVal::new("custom", "retry: no attempts made");
        for attempt in 0..n {
            match self.call_value(&thunk, CallArgs::default()) {
                Ok(v) => return Ok(v),
                Err(e) => {
                    last = e;
                    if attempt + 1 < n
                        && let Some(ns) = delay
                        && ns > 0
                    {
                        std::thread::sleep(Duration::from_nanos(ns as u64));
                    }
                }
            }
        }
        Err(last)
    }

    /// `run(<path>, …)` / `run(<name>, …)` — the poly runner + dynamic form.
    fn run_poly(&mut self, target: Value, args: Vec<Value>, position: Position) -> VResult<Value> {
        let name = match &target {
            Value::Str(s) => s.clone(),
            Value::Path(p) => p.to_string_lossy().into_owned(),
            v => {
                return Err(ErrorVal::type_error(format!(
                    "run expects a str or path, found {}",
                    v.type_name()
                )));
            }
        };
        let is_path =
            name.contains('/') || name.starts_with('.') || name.starts_with('~');
        let resolved = {
            let p = self.resolve_path(&name);
            if p.is_absolute() { p } else { self.cwd.join(p) }
        };
        let ext = Path::new(&name)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        let scripty = matches!(ext.as_deref(), Some("shl" | "sh" | "py" | "js" | "rs"));
        if is_path || (scripty && resolved.exists()) {
            return self.run_script_file(&resolved, ext.as_deref(), args, position);
        }
        // Dynamic command invocation (value semantics like any command).
        let mut argv = vec![OsString::from(&name)];
        for v in args {
            argv.push(self.argv_value(v)?);
        }
        self.run_argv(argv, position, StdinSpec::Null, &[], Span::default(), None)
    }

    fn run_script_file(
        &mut self,
        path: &Path,
        ext: Option<&str>,
        args: Vec<Value>,
        position: Position,
    ) -> VResult<Value> {
        match ext {
            Some("shl") | None => {
                let src = std::fs::read_to_string(path)
                    .map_err(|e| ErrorVal::new("io_error", format!("cannot read script: {e}")))?;
                let program = shoal_syntax::parse(&src)
                    .map_err(|e| ErrorVal::new("parse_error", e.to_string()))?;
                let mut child = Evaluator::new(self.cwd.clone());
                child.env = self.env.clone();
                child.process_env = self.process_env.clone();
                child.adapters = self.adapters.clone();
                child.env.declare("args", Value::List(args), false);
                child.eval_program(&program)
            }
            Some("sh") => self.run_interp("sh", path, args, position),
            Some("py") => self.run_interp("python3", path, args, position),
            Some("js") => self.run_interp("node", path, args, position),
            Some("rs") => self.run_rust_script(path, args, position),
            Some(_) => {
                let mut argv = vec![path.as_os_str().to_owned()];
                for v in args {
                    argv.push(self.argv_value(v)?);
                }
                self.run_argv(argv, position, StdinSpec::Null, &[], Span::default(), None)
            }
        }
    }

    fn run_interp(
        &mut self,
        interp: &str,
        path: &Path,
        args: Vec<Value>,
        position: Position,
    ) -> VResult<Value> {
        let mut argv = vec![OsString::from(interp), path.as_os_str().to_owned()];
        for v in args {
            argv.push(self.argv_value(v)?);
        }
        self.run_argv(argv, position, StdinSpec::Null, &[], Span::default(), None)
    }

    fn run_rust_script(
        &mut self,
        path: &Path,
        args: Vec<Value>,
        position: Position,
    ) -> VResult<Value> {
        let path_env = self
            .process_env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.as_os_str());
        if shoal_exec::which(OsStr::new("rust-script"), path_env).is_some() {
            return self.run_interp("rust-script", path, args, position);
        }
        // Fall back to compiling with rustc into a temp binary, then exec it.
        let bin = std::env::temp_dir().join(format!(
            "shoal-rs-{}-{}",
            std::process::id(),
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("script")
        ));
        let compile = self.run_argv(
            vec![
                OsString::from("rustc"),
                path.as_os_str().to_owned(),
                OsString::from("-o"),
                bin.clone().into_os_string(),
            ],
            Position::Value,
            StdinSpec::Null,
            &[],
            Span::default(),
            None,
        )?;
        if let Value::Outcome(o) = &compile
            && !o.ok
        {
            return Err(ErrorVal::new(
                "cmd_failed",
                format!(
                    "rustc failed to compile {}: {}",
                    path.display(),
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
            ));
        }
        let mut argv = vec![bin.into_os_string()];
        for v in args {
            argv.push(self.argv_value(v)?);
        }
        self.run_argv(argv, position, StdinSpec::Null, &[], Span::default(), None)
    }

    /// `.pick()` — interactive fuzzy selection via shoal-picker, gated on a tty.
    fn pick(&self, recv: Value, args: &CallArgs) -> VResult<Value> {
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

/// Wrap a builtin's structured result in a `Value::Outcome` (outcome
/// unification, P1a). The structured value becomes the outcome's `parsed`
/// (`.out`); `stdout` carries the same bytes a redirect/`echo … > file` would
/// write, so `echo`, `ls`, `stat`, `which`, … all compose and forward like
/// external outcomes. Builtin outcomes are marked `pid == 0` so the statement
/// sink knows they were never PtyTee-streamed.
fn builtin_outcome(head: &str, result: Value) -> Value {
    let stdout = value_bytes(&result);
    Value::Outcome(Arc::new(OutcomeVal {
        status: Some(0),
        signal: None,
        ok: true,
        stdout: Arc::new(stdout),
        stderr: Arc::new(Vec::new()),
        dur_ns: 0,
        pid: 0,
        cmd: head.to_string(),
        parsed: Some(result),
    }))
}

/// Render a value to bytes for a builtin redirect target (defect #8).
fn value_bytes(v: &Value) -> Vec<u8> {
    match v {
        Value::Bytes(b) => (**b).clone(),
        Value::Str(s) => {
            let mut b = s.clone().into_bytes();
            if !s.ends_with('\n') {
                b.push(b'\n');
            }
            b
        }
        Value::Outcome(o) => (*o.stdout).clone(),
        Value::Null => Vec::new(),
        other => {
            let mut b = display_top(other).into_bytes();
            b.push(b'\n');
            b
        }
    }
}

/// A statement is a "bare command" when its root is a command invocation
/// (or a boolean composition of commands) — defect #1b / WP1's Binary{And,Cmd,Cmd}.
fn is_command_expr(e: &Expr) -> bool {
    match e {
        Expr::Cmd { .. } | Expr::ShRaw { .. } => true,
        Expr::Binary {
            op: BinOp::And | BinOp::Or,
            lhs,
            rhs,
            ..
        } => is_command_expr(lhs) && is_command_expr(rhs),
        _ => false,
    }
}

/// Top-level display of a value for the default statement sink and for `echo`:
/// strings/paths are unquoted at the top level; nested values use `render_inline`.
fn display_top(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Path(p) => p.to_string_lossy().into_owned(),
        Value::Null => String::new(),
        other => shoal_value::render::render_inline(other),
    }
}

/// Coerce a single CMD word value to a declared parameter type (TDD §4.2 site 2,
/// defect #12). Non-string values pass through unchanged; unknown types keep the
/// value verbatim (→ str).
fn coerce_word(v: Value, ty: &str) -> VResult<Value> {
    let ty = ty.trim_end_matches('?');
    let Value::Str(s) = &v else {
        return Ok(v);
    };
    let s = s.clone();
    match ty {
        "str" => Ok(Value::Str(s)),
        "int" => s
            .parse::<i64>()
            .map(Value::Int)
            .map_err(|_| ErrorVal::arg_error(format!("expected int, found {s:?}"))),
        "float" => s
            .parse::<f64>()
            .map(Value::Float)
            .map_err(|_| ErrorVal::arg_error(format!("expected float, found {s:?}"))),
        "size" => shoal_value::parse_size(&s)
            .map(Value::Size)
            .ok_or_else(|| ErrorVal::arg_error(format!("expected size, found {s:?}"))),
        "duration" => {
            // Accept bare integers as seconds so `sleep 1` and `sleep 10ms` both work.
            if let Some(ns) = shoal_value::parse_duration(&s) {
                Ok(Value::Duration(ns))
            } else if let Ok(secs) = s.parse::<i64>() {
                Ok(Value::Int(secs))
            } else {
                Err(ErrorVal::arg_error(format!(
                    "expected duration, found {s:?}"
                )))
            }
        }
        "time" => shoal_value::parse_time(&s)
            .map(Value::Time)
            .ok_or_else(|| ErrorVal::arg_error(format!("expected time, found {s:?}"))),
        "datetime" => parse_datetime(&s)
            .map(|z| Value::DateTime(Box::new(z)))
            .map_err(|_| ErrorVal::arg_error(format!("expected datetime, found {s:?}"))),
        "bool" => match s.as_str() {
            "true" => Ok(Value::Bool(true)),
            "false" => Ok(Value::Bool(false)),
            _ => Err(ErrorVal::arg_error(format!("expected bool, found {s:?}"))),
        },
        "path" => Ok(Value::Path(PathBuf::from(s))),
        "glob" => Ok(Value::Glob(shoal_value::GlobVal {
            pattern: s,
            cwd: PathBuf::new(),
            hidden: false,
        })),
        _ => Ok(Value::Str(s)),
    }
}

/// Item type name a positional slot at `idx` coerces to: the declared param's
/// type name, or — once `idx` runs past the fixed params — the `...rest`
/// param's element type (unwrapping a `list<T>` annotation to `T`), so
/// `...nums: list<int>` and `...nums: int` both accumulate as `int`.
fn expected_param_ty<'a>(
    params: &'a [Param],
    rest: Option<&'a RestParam>,
    idx: usize,
) -> Option<&'a str> {
    if let Some(p) = params.get(idx) {
        return p.ty.as_ref().map(|t| t.name.as_str());
    }
    rest.and_then(|r| r.ty.as_ref()).map(|t| {
        if t.name == "list" {
            t.args.first().map(|a| a.name.as_str()).unwrap_or("str")
        } else {
            t.name.as_str()
        }
    })
}

/// Coerce positional + named CMD-word arguments against a function's parameters
/// (TDD §4.2 site 2 / §4.4 `...rest`). Variadic tails accumulate: every
/// positional word beyond the fixed params is coerced to the rest param's
/// element type before `call_value_inner` collects them into a `list`.
fn coerce_call_args(
    params: &[Param],
    rest: Option<&RestParam>,
    pos: &mut [Value],
    named: &mut [(String, Value)],
) -> VResult<()> {
    for (i, p) in params.iter().enumerate() {
        let Some(ty) = &p.ty else { continue };
        // list<T> accumulation is not yet handled here; leave those verbatim.
        if ty.name == "list" {
            continue;
        }
        if let Some(slot) = named.iter_mut().find(|(n, _)| n == &p.name) {
            slot.1 = coerce_word(std::mem::replace(&mut slot.1, Value::Null), &ty.name)?;
        } else if let Some(slot) = pos.get_mut(i) {
            *slot = coerce_word(std::mem::replace(slot, Value::Null), &ty.name)?;
        }
    }
    if rest.is_some()
        && let Some(item_ty) = expected_param_ty(params, rest, params.len())
    {
        for slot in pos.iter_mut().skip(params.len()) {
            *slot = coerce_word(std::mem::replace(slot, Value::Null), item_ty)?;
        }
    }
    Ok(())
}

/// Synthesised `--help` text for a user fn (§4.4).
fn closure_help(c: &shoal_value::ClosureVal) -> String {
    let name = c.name.clone().unwrap_or_else(|| "fn".into());
    let mut params: Vec<String> = c
        .params
        .iter()
        .map(|p| match &p.ty {
            Some(t) => format!("{}: {}", p.name, t.name),
            None => p.name.clone(),
        })
        .collect();
    if let Some(rest) = &c.rest {
        params.push(format!("...{}", rest.name));
    }
    let mut out = format!("{name}({})", params.join(", "));
    if let Some(ret) = &c.ret {
        out.push_str(&format!(" -> {}", ret.name));
    }
    if let Some(doc) = &c.doc {
        out.push('\n');
        out.push_str(doc);
    }
    out
}

/// Default statement sink (no host wired): print command output to real stdout.
fn default_render(v: &Value) {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    match v {
        Value::Outcome(o) => {
            let _ = lock.write_all(&o.stdout);
        }
        other => {
            let _ = writeln!(lock, "{}", display_top(other));
        }
    }
}

fn signature(spec: &SubSpec) -> String {
    spec.params
        .iter()
        .map(|p| format!("--{} <{}>", p.name.replace('_', "-"), p.ty))
        .collect::<Vec<_>>()
        .join(", ")
}

fn validate_adapter_value(value: &Value, ty: &str) -> VResult<()> {
    let ty = ty.trim_end_matches('?');
    let valid = match ty {
        "str" => matches!(value, Value::Str(_)),
        "bool" => matches!(value, Value::Bool(_) | Value::Str(_)),
        "int" => {
            matches!(value, Value::Int(_))
                || matches!(value, Value::Str(s) if s.parse::<i64>().is_ok())
        }
        "float" => {
            matches!(value, Value::Int(_) | Value::Float(_))
                || matches!(value, Value::Str(s) if s.parse::<f64>().is_ok())
        }
        "path" => matches!(value, Value::Path(_) | Value::Str(_)),
        "glob" => matches!(value, Value::Glob(_) | Value::Str(_)),
        "size" => {
            matches!(value, Value::Size(_))
                || matches!(value, Value::Str(s) if shoal_value::parse_size(s).is_some())
        }
        "duration" => {
            matches!(value, Value::Duration(_))
                || matches!(value, Value::Str(s) if shoal_value::parse_duration(s).is_some())
        }
        "time" => {
            matches!(value, Value::Time(_))
                || matches!(value, Value::Str(s) if shoal_value::parse_time(s).is_some())
        }
        ty if ty.starts_with("list<") => true,
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(ErrorVal::arg_error(format!(
            "expected {ty}, found {}",
            value.type_name()
        )))
    }
}

fn push_effect(out: &mut Vec<Effect>, effect: Effect) {
    if !out.contains(&effect) {
        out.push(effect)
    }
}
fn plan_text(arg: &CmdArg) -> VResult<String> {
    match arg {
        CmdArg::Word { text, .. }
        | CmdArg::Path { text, .. }
        | CmdArg::Glob { pattern: text, .. } => Ok(text.clone()),
        CmdArg::Str { expr, .. } | CmdArg::Expr { expr, .. } => match expr {
            Expr::Str { value, .. } => Ok(value.clone()),
            Expr::Int { value, .. } => Ok(value.to_string()),
            _ => Err(ErrorVal::arg_error("planning requires a literal argument")),
        },
        _ => Err(ErrorVal::arg_error("planning requires a value argument")),
    }
}
fn plan_paths(arg: &CmdArg, cwd: &Path) -> VResult<Vec<PathBuf>> {
    match arg {
        CmdArg::Glob { pattern, .. } => {
            let pat = cwd.join(pattern).to_string_lossy().into_owned();
            let mut ps = glob::glob(&pat)
                .map_err(|e| ErrorVal::arg_error(e.to_string()))?
                .filter_map(Result::ok)
                .collect::<Vec<_>>();
            ps.sort();
            Ok(ps)
        }
        _ => {
            let p = PathBuf::from(plan_text(arg)?);
            Ok(vec![if p.is_absolute() { p } else { cwd.join(p) }])
        }
    }
}
fn parse_declared_effect(
    raw: &str,
    bindings: &std::collections::HashMap<String, Vec<String>>,
    cwd: &Path,
) -> Vec<Effect> {
    let Some((kind, arg)) = raw
        .split_once('(')
        .and_then(|(k, a)| a.strip_suffix(')').map(|a| (k, a)))
    else {
        return vec![];
    };
    let values = if arg == "cwd" {
        vec![cwd.to_string_lossy().into_owned()]
    } else if let Some(key) = arg.strip_prefix('$') {
        bindings.get(key).cloned().unwrap_or_default()
    } else {
        vec![arg.to_owned()]
    };
    match kind {
        "fs.read" => vec![Effect::FsRead {
            paths: values
                .into_iter()
                .map(|p| {
                    let p = PathBuf::from(p);
                    if p.is_absolute() { p } else { cwd.join(p) }
                })
                .collect(),
        }],
        "fs.write" => vec![Effect::FsWrite {
            paths: values
                .into_iter()
                .map(|p| {
                    let p = PathBuf::from(p);
                    if p.is_absolute() { p } else { cwd.join(p) }
                })
                .collect(),
        }],
        "fs.delete" => vec![Effect::FsDelete {
            paths: values
                .into_iter()
                .map(|p| {
                    let p = PathBuf::from(p);
                    if p.is_absolute() { p } else { cwd.join(p) }
                })
                .collect(),
        }],
        "net.connect" => values
            .into_iter()
            .map(|v| {
                let (host, port) = v
                    .rsplit_once(':')
                    .and_then(|(h, p)| p.parse().ok().map(|p| (h.to_owned(), p)))
                    .unwrap_or((v, 443));
                Effect::NetConnect { host, port }
            })
            .collect(),
        _ => vec![],
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

fn parse_datetime(iso: &str) -> VResult<jiff::Zoned> {
    if let Ok(zoned) = iso.parse::<jiff::Zoned>() {
        return Ok(zoned);
    }
    if let Ok(timestamp) = iso.parse::<jiff::Timestamp>() {
        return Ok(timestamp.to_zoned(jiff::tz::TimeZone::UTC));
    }
    if let Ok(date) = iso.parse::<jiff::civil::Date>() {
        return date
            .to_zoned(jiff::tz::TimeZone::UTC)
            .map_err(|e| ErrorVal::new("arg_error", format!("invalid datetime: {e}")));
    }
    Err(ErrorVal::new(
        "arg_error",
        format!("invalid datetime `{iso}`"),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> VResult<Value> {
        let program = shoal_syntax::parse(src).unwrap_or_else(|e| panic!("parse failed: {e}"));
        eval(&program, std::env::current_dir().unwrap())
    }

    /// Evaluate `src` capturing everything routed to the statement sink.
    fn run_capturing(src: &str) -> (VResult<Value>, Vec<Value>) {
        use std::sync::{Arc, Mutex};
        let program = shoal_syntax::parse(src).unwrap_or_else(|e| panic!("parse failed: {e}"));
        let mut ev = Evaluator::new(std::env::current_dir().unwrap());
        let sink: Arc<Mutex<Vec<Value>>> = Arc::default();
        let sink2 = sink.clone();
        ev.set_statement_sink(Box::new(move |v: &Value| sink2.lock().unwrap().push(v.clone())));
        let out = ev.eval_program(&program);
        drop(ev); // release the sink's Arc clone before unwrapping
        let captured = Arc::try_unwrap(sink).unwrap().into_inner().unwrap();
        (out, captured)
    }

    fn run_in(src: &str, cwd: &Path) -> VResult<Value> {
        let program = shoal_syntax::parse(src).unwrap_or_else(|e| panic!("parse failed: {e}"));
        eval(&program, cwd)
    }

    /// The structured `.out` of a captured command outcome.
    fn out_of(v: &Value) -> Value {
        match v {
            Value::Outcome(o) => o.out_value(),
            other => other.clone(),
        }
    }

    #[test]
    fn defect1_nonfinal_and_block_commands_reach_sink() {
        // Non-final top-level statement values pass through to the sink; the
        // final value is returned. Every command now yields an outcome whose
        // `.out` carries the joined echo text (outcome unification, P1a).
        let (out, captured) = run_capturing("echo hi\necho bye");
        assert_eq!(out_of(&out.unwrap()), Value::Str("bye".into()));
        assert_eq!(captured.len(), 1);
        assert_eq!(out_of(&captured[0]), Value::Str("hi".into()));

        // Every iteration of a loop body's bare command reaches the sink.
        let (_out, captured) = run_capturing("for x in [1,2,3] { echo (x) }");
        let texts: Vec<Value> = captured.iter().map(out_of).collect();
        assert_eq!(
            texts,
            vec![
                Value::Str("1".into()),
                Value::Str("2".into()),
                Value::Str("3".into()),
            ]
        );
    }

    #[test]
    fn outcome_unification_builtin_out_and_ok() {
        // A builtin is an outcome: `.out` is its structured result, `.ok` true.
        assert_eq!(run("(echo hi).out").unwrap(), Value::Str("hi".into()));
        assert_eq!(run("(echo hi).ok").unwrap(), Value::Bool(true));
        // Unknown fields forward to `.out` (stat record → `.size`).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"xyz").unwrap();
        assert_eq!(run_in("(stat a).size", dir.path()).unwrap(), Value::Size(3));
    }

    #[test]
    fn outcome_unification_and_or_compose_commands() {
        // `echo a && echo b` prints BOTH (P1d): `a` via the sink, `b` returned.
        let (out, captured) = run_capturing("echo a && echo b");
        assert_eq!(out_of(&out.unwrap()), Value::Str("b".into()));
        assert_eq!(captured.iter().map(out_of).collect::<Vec<_>>(), vec![Value::Str("a".into())]);
        // A three-stage chain prints every stage.
        let (out, captured) = run_capturing("echo a && echo b && echo c");
        assert_eq!(out_of(&out.unwrap()), Value::Str("c".into()));
        assert_eq!(
            captured.iter().map(out_of).collect::<Vec<_>>(),
            vec![Value::Str("a".into()), Value::Str("b".into())]
        );
        // `||` recovers from a failed command without raising.
        let out = run("sh { exit 1 } || echo x").unwrap();
        assert_eq!(out_of(&out), Value::Str("x".into()));
    }

    #[test]
    fn outcome_forwards_collection_methods() {
        // `ls` is an outcome; `.where`/`.sort`/`.first(n)`/`.map` forward to its
        // `.out` table (outcome unification P1b + first(n) arity fix).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("big"), vec![0u8; 2048]).unwrap();
        std::fs::write(dir.path().join("small"), b"x").unwrap();
        let names = run_in("ls.where(.size > 1b).sort(.name).map(.name)", dir.path()).unwrap();
        assert_eq!(names, Value::List(vec![Value::Path("big".into())]));
        // `.first(2)` returns a LIST of two, chainable into `.map`.
        std::fs::write(dir.path().join("mid"), vec![0u8; 4]).unwrap();
        let first_two = run_in("ls.sort(.name).first(2).map(.name)", dir.path()).unwrap();
        assert!(matches!(first_two, Value::List(xs) if xs.len() == 2));
    }

    #[test]
    fn double_echo_fixed_and_bare_echo_blank_line() {
        // A fn whose last body statement is a bare command prints ONCE: the
        // trailing command is the block value, not also sunk (P1 dbl-echo).
        let (out, captured) = run_capturing("fn g(){ echo hi }\ng()");
        assert_eq!(out_of(&out.unwrap()), Value::Str("hi".into()));
        assert!(captured.is_empty(), "trailing command must not double-print: {captured:?}");
        // Bare `echo` emits a blank line: its outcome stdout is "\n".
        let (_out, captured) = run_capturing("echo\n42");
        assert_eq!(captured.len(), 1);
        match &captured[0] {
            Value::Outcome(o) => assert_eq!(&*o.stdout, b"\n"),
            other => panic!("expected outcome, got {other:?}"),
        }
    }

    #[test]
    fn top_level_ls_renders_as_table() {
        // An outcome with a structured `.out` renders as that structure (P1c).
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("only"), b"x").unwrap();
        let v = run_in("ls", dir.path()).unwrap();
        let rendered = shoal_value::render::render_block(&v, 80);
        assert!(rendered.contains("name"), "ls should render a table: {rendered:?}");
        assert!(rendered.contains("only"), "ls table should list the file: {rendered:?}");
    }

    #[test]
    fn defect3_forced_command_still_resolves_session_fn() {
        assert_eq!(
            run("fn greet(n:str){ (n) }\n^greet world").unwrap(),
            Value::Str("world".into())
        );
    }

    #[test]
    fn defect4_stat_modified_is_datetime() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"x").unwrap();
        let v = run_in("stat a", dir.path()).unwrap();
        let Value::Record(r) = out_of(&v) else { panic!("stat should be a record") };
        assert!(
            matches!(r.get("modified"), Some(Value::DateTime(_))),
            "modified must be a DateTime, got {:?}",
            r.get("modified")
        );
    }

    #[test]
    fn defect5_command_resolves_in_value_position() {
        // `let r = ls` invokes the builtin zero-arg in value position.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"x").unwrap();
        let v = run_in("let r = ls\nr", dir.path()).unwrap();
        // `ls` now yields an outcome; its `.out` is the table (P1a).
        assert!(matches!(out_of(&v), Value::Table(rows) if rows.len() == 1));
    }

    #[test]
    fn defect5_env_field_read_via_command() {
        // `env.PATH` reads by invoking the `env` builtin then projecting.
        unsafe { std::env::set_var("SHOAL_TEST_VAR", "hello") };
        let v = run("env.SHOAL_TEST_VAR").unwrap();
        assert_eq!(v, Value::Str("hello".into()));
    }

    #[test]
    fn defect8_redirect_applies_to_builtin() {
        let dir = tempfile::tempdir().unwrap();
        run_in("echo hi > b.txt", dir.path()).unwrap();
        let body = std::fs::read_to_string(dir.path().join("b.txt")).unwrap();
        assert_eq!(body, "hi\n");
    }

    #[test]
    fn defect9_recursion_guard_returns_error() {
        // Run on a large stack so the depth guard fires before the native stack
        // overflows (the real binary evaluates on a big main-thread stack).
        let code = std::thread::Builder::new()
            .stack_size(2 * 1024 * 1024 * 1024)
            .spawn(|| run("fn rec(n:int){ rec(n) }\nrec(1)").unwrap_err().code)
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(code, "recursion_limit");
    }

    #[test]
    fn defect10_cd_inside_fn_body_is_rejected() {
        let err = run("fn f(){ cd / }\nf()").unwrap_err();
        assert_eq!(err.code, "custom");
        assert!(err.msg.contains("with cwd:"), "{}", err.msg);
    }

    #[test]
    fn defect11_env_assignment_writes_session_env() {
        use shoal_ast::*;
        let s = Span::default();
        let target = Expr::Field {
            recv: Box::new(Expr::Var {
                name: "env".into(),
                span: s,
            }),
            name: "SHOAL_ASSIGNED".into(),
            optional: false,
            span: s,
        };
        let program = Program {
            stmts: vec![
                Stmt::Assign {
                    target,
                    op: AssignOp::Set,
                    value: Expr::Str {
                        value: "bar".into(),
                        span: s,
                    },
                    span: s,
                },
                Stmt::Expr {
                    expr: Expr::Field {
                        recv: Box::new(Expr::Var {
                            name: "env".into(),
                            span: s,
                        }),
                        name: "SHOAL_ASSIGNED".into(),
                        optional: false,
                        span: s,
                    },
                    span: s,
                },
            ],
        };
        let v = eval(&program, std::env::current_dir().unwrap()).unwrap();
        assert_eq!(v, Value::Str("bar".into()));
    }

    #[test]
    fn defect11_env_assignment_rejected_in_fn_body() {
        use shoal_ast::*;
        let s = Span::default();
        let assign = Stmt::Assign {
            target: Expr::Field {
                recv: Box::new(Expr::Var {
                    name: "env".into(),
                    span: s,
                }),
                name: "X".into(),
                optional: false,
                span: s,
            },
            op: AssignOp::Set,
            value: Expr::Str {
                value: "1".into(),
                span: s,
            },
            span: s,
        };
        // fn f() { env.X = "1" }  then  f()
        let decl = FnDecl {
            name: "f".into(),
            params: vec![],
            rest: None,
            ret: None,
            body: Block {
                stmts: vec![assign],
                span: s,
            },
            doc: None,
            exported: false,
            span: s,
        };
        let program = Program {
            stmts: vec![
                Stmt::Fn { decl },
                Stmt::Expr {
                    expr: Expr::FnCall {
                        name: "f".into(),
                        args: Args::empty(),
                        span: s,
                    },
                    span: s,
                },
            ],
        };
        let err = eval(&program, std::env::current_dir().unwrap()).unwrap_err();
        assert!(err.msg.contains("with env:"), "{}", err.msg);
    }

    #[test]
    fn defect12_builtin_word_coercion() {
        // `sleep 0ms` binds the word to a duration; `sleep 0` to seconds. The
        // builtin now yields an outcome whose `.out` is null (P1a).
        assert_eq!(out_of(&run("sleep 0ms").unwrap()), Value::Null);
        assert_eq!(out_of(&run("sleep 0").unwrap()), Value::Null);
    }

    #[test]
    fn defect12_fn_param_word_coercion() {
        // A bare CMD word binds to a typed fn param.
        let v = run("fn add1(n: int) { n + 1 }\nadd1 41").unwrap();
        assert_eq!(v, Value::Int(42));
    }

    #[test]
    fn defect12_help_synthesis_returns_null() {
        let (out, captured) = run_capturing("fn deploy(env: str) { (env) }\ndeploy --help");
        assert_eq!(out.unwrap(), Value::Null);
        assert!(
            matches!(captured.last(), Some(Value::Str(s)) if s.contains("deploy") && s.contains("env")),
            "{captured:?}"
        );
    }

    #[test]
    fn defect14_task_methods_and_jobs() {
        assert_eq!(
            run("let t = spawn { 2 + 3 }\nt.await()").unwrap(),
            Value::Int(5)
        );
        let is_done = run("let t = spawn { 7 }\nt.await()\nt.is_done()").unwrap();
        assert_eq!(is_done, Value::Bool(true));
        // `jobs` returns the registry table.
        let jobs = run("spawn { 1 }\njobs").unwrap();
        assert!(matches!(jobs, Value::Table(rows) if !rows.is_empty()));
    }

    #[test]
    fn echo_renders_non_scalar_values() {
        let v = run("let items = [1,2,3]\necho (items)").unwrap();
        assert_eq!(out_of(&v), Value::Str("[1, 2, 3]".into()));
    }

    #[test]
    fn record_transcript_binds_it_and_out() {
        let mut ev = Evaluator::new(std::env::current_dir().unwrap());
        ev.record_transcript(&Value::Int(7));
        ev.record_transcript(&Value::Str("hi".into()));
        let it = ev
            .eval_program(&shoal_syntax::parse("it").unwrap())
            .unwrap();
        assert_eq!(it, Value::Str("hi".into()));
        let out = ev
            .eval_program(&shoal_syntax::parse("out").unwrap())
            .unwrap();
        assert_eq!(out, Value::List(vec![Value::Int(7), Value::Str("hi".into())]));
    }

    #[test]
    fn builtin_retry_and_parallel_and_save() {
        assert_eq!(run("retry(3, () => 42)").unwrap(), Value::Int(42));
        assert_eq!(
            run("parallel(() => 1, () => 2)").unwrap(),
            Value::List(vec![Value::Int(1), Value::Int(2)])
        );
        let dir = tempfile::tempdir().unwrap();
        run_in("save(\"out.txt\", \"payload\")", dir.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "payload"
        );
    }

    #[test]
    fn builtin_retry_eventually_surfaces_error() {
        let err = run("retry(2, () => missing_command_xyz)").unwrap_err();
        assert!(err.code == "undefined_var" || err.code == "not_found", "{}", err.code);
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

    #[test]
    fn typed_builtins_dispatch_before_path() {
        let dir = tempfile::tempdir().unwrap();
        let program = shoal_syntax::parse("touch a\nls").unwrap();
        let value = out_of(&eval(&program, dir.path()).unwrap());
        assert!(
            matches!(value, Value::Table(rows) if rows.len() == 1 && rows[0]["name"] == Value::Path("a".into()))
        );

        let rm = shoal_syntax::parse("rm a").unwrap();
        let value = out_of(&eval(&rm, dir.path()).unwrap());
        assert!(
            matches!(value, Value::List(rows) if matches!(&rows[0], Value::Record(r) if matches!(r.get("trash"), Some(Value::Path(_)))))
        );
        assert!(!dir.path().join("a").exists());
    }

    fn adapter_eval(toml: &str, src: &str) -> VResult<Value> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fixture.toml"), toml).unwrap();
        let (catalog, warnings) = AdapterCatalog::load_dir(dir.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut evaluator = Evaluator::new(dir.path().into());
        evaluator.set_adapters(catalog);
        evaluator.eval_program(&shoal_syntax::parse(src).unwrap())
    }

    #[test]
    fn adapters_rewrite_parse_and_honor_ok_codes() {
        let lines = adapter_eval(
            r#"[cmd.fixture]
bin="/usr/bin/printf"
invoke=["one\ntwo\n"]
output={parse="lines",type="list<str>"}
"#,
            "fixture",
        )
        .unwrap();
        assert!(
            matches!(lines, Value::Outcome(o) if o.out_value() == Value::List(vec![Value::Str("one".into()), Value::Str("two".into())]))
        );

        let accepted = adapter_eval(
            r#"[cmd.accept]
bin="/bin/sh"
ok_codes=[0,1]
invoke=["-c","exit 1"]
"#,
            "accept",
        )
        .unwrap();
        assert!(matches!(accepted, Value::Outcome(o) if o.ok && o.status == Some(1)));
    }

    #[test]
    fn adapter_typed_flags_fail_before_spawn() {
        let error = adapter_eval(
            r#"[cmd.typed]
bin="/usr/bin/printf"
params={jobs="int"}
"#,
            "typed --jobs=nope",
        )
        .unwrap_err();
        assert_eq!(error.code, "arg_error");
        assert!(error.msg.contains("expected int"));
    }

    #[test]
    fn adapter_consumed_flag_never_reaches_argv() {
        // Regression for the git-status porcelain corruption (shoal-adapters'
        // `consumed` rule, defect fix): `--short`/`-s` must stay a
        // recognized, validated flag but never be appended to argv, since
        // git's `--porcelain=v2` parser assumes an exact byte layout and
        // `--short` (last-wins) silently switches git to a different,
        // incompatible output format.
        let toml = r#"[cmd.fixture]
bin="/bin/echo"

[cmd.fixture.sub.status]
params = { short = "bool", branch = "bool" }
flags = { short = { s = "short", b = "branch" } }
invoke = ["status", "--porcelain=v2"]
consumed = ["short", "branch"]
"#;

        let long = adapter_eval(toml, "fixture status --short").unwrap();
        let Value::Outcome(o) = long else {
            panic!("expected outcome, got {long:?}")
        };
        assert_eq!(
            String::from_utf8(o.stdout.to_vec()).unwrap().trim(),
            "status --porcelain=v2",
            "--short must be accepted but dropped from argv"
        );

        let short = adapter_eval(toml, "fixture status -s").unwrap();
        let Value::Outcome(o) = short else {
            panic!("expected outcome, got {short:?}")
        };
        assert_eq!(
            String::from_utf8(o.stdout.to_vec()).unwrap().trim(),
            "status --porcelain=v2",
            "-s must be accepted but dropped from argv"
        );
    }

    #[test]
    fn planning_derives_exact_builtin_paths_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"a").unwrap();
        let mut evaluator = Evaluator::new(dir.path().into());
        let program = shoal_syntax::parse("cp a b\nrm a").unwrap();
        let plan = evaluator.plan_program(&program).unwrap();
        assert!(plan.effects.contains(&Effect::FsRead {
            paths: vec![dir.path().join("a")]
        }));
        assert!(plan.effects.contains(&Effect::FsWrite {
            paths: vec![dir.path().join("b")]
        }));
        assert!(plan.effects.contains(&Effect::FsDelete {
            paths: vec![dir.path().join("a")]
        }));
        assert!(dir.path().join("a").exists());
        assert!(!dir.path().join("b").exists());
    }

    #[test]
    fn planning_substitutes_adapter_effects() {
        let dir = tempfile::tempdir().unwrap();
        let mut evaluator = Evaluator::new(dir.path().into());
        assert!(evaluator.load_bundled_adapters().is_empty());
        let plan = evaluator
            .plan_program(&shoal_syntax::parse("git push origin main").unwrap())
            .unwrap();
        assert!(plan.effects.contains(&Effect::FsRead {
            paths: vec![dir.path().into()]
        }));
        assert!(plan.effects.contains(&Effect::NetConnect {
            host: "origin".into(),
            port: 443
        }));
        assert!(
            plan.effects
                .iter()
                .any(|e| matches!(e, Effect::ProcSpawn { argv0, .. } if argv0 == "git"))
        );
    }

    #[test]
    fn planning_unknown_and_sh_are_opaque_and_spawn_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let src = format!("unknown-command\nsh {{ touch {} }}", marker.display());
        let mut evaluator = Evaluator::new(dir.path().into());
        let plan = evaluator
            .plan_program(&shoal_syntax::parse(&src).unwrap())
            .unwrap();
        assert!(plan.effects.contains(&Effect::Opaque));
        assert!(!marker.exists());
    }

    #[test]
    fn planning_unions_conditional_and_static_function_effects() {
        let dir = tempfile::tempdir().unwrap();
        let src = "fn cleanup() { rm old }\nif true { cleanup() } else { touch new }";
        let mut evaluator = Evaluator::new(dir.path().into());
        let parsed = shoal_syntax::parse(src).unwrap();
        let plan = evaluator.plan_program(&parsed).unwrap();
        assert!(plan.effects.contains(&Effect::FsDelete {
            paths: vec![dir.path().join("old")]
        }));
        assert!(plan.effects.contains(&Effect::FsWrite {
            paths: vec![dir.path().join("new")]
        }));
    }

    // --- match: type / record / list patterns (TDD §3.2) -----------------

    #[test]
    fn match_type_pattern_binds_and_falls_through() {
        assert_eq!(
            run(r#"match 5 { int n => "int:{n}"; _ => "other" }"#).unwrap(),
            Value::Str("int:5".into())
        );
        assert_eq!(
            run(r#"match "hi" { str s => "str:{s}"; _ => "other" }"#).unwrap(),
            Value::Str("str:hi".into())
        );
        // A type mismatch falls through to the next arm.
        assert_eq!(
            run(r#"match "hi" { int n => "int:{n}"; str s => "str:{s}" }"#).unwrap(),
            Value::Str("str:hi".into())
        );
        // A bare type name with no binder is a plain bind (matches anything).
        assert_eq!(run(r#"match 5 { int => 1; _ => 0 }"#).unwrap(), Value::Int(1));
    }

    #[test]
    fn match_record_pattern_shorthand_sub_and_open() {
        assert_eq!(
            run(r#"match {name: "ada", age: 30} { {name, age} => "{name} is {age}"; _ => "no" }"#)
                .unwrap(),
            Value::Str("ada is 30".into())
        );
        // Nested record sub-pattern.
        assert_eq!(
            run("match {point: {x: 1, y: 2}} { {point: {x, y}} => x + y; _ => 0 }").unwrap(),
            Value::Int(3)
        );
        // Missing field falls through (open matching only ignores *extra*).
        assert_eq!(
            run(r#"match {name: "ada"} { {name, age} => "has age"; _ => "no age" }"#).unwrap(),
            Value::Str("no age".into())
        );
        // Record + nested list sub-pattern.
        assert_eq!(
            run("match {items: [1, 2, 3]} { {items: [a, b, c]} => a + b + c; _ => 0 }").unwrap(),
            Value::Int(6)
        );
    }

    #[test]
    fn match_record_pattern_guard_composes() {
        assert_eq!(
            run(r#"match {status: 200} { {status} if status >= 200 && status < 300 => "ok"; {status} => "other:{status}" }"#)
                .unwrap(),
            Value::Str("ok".into())
        );
        assert_eq!(
            run(r#"match {status: 404} { {status} if status >= 200 && status < 300 => "ok"; {status} => "other:{status}" }"#)
                .unwrap(),
            Value::Str("other:404".into())
        );
    }

    #[test]
    fn match_list_pattern_arity_rest_and_empty() {
        assert_eq!(
            run("match [1, 2, 3] { [a, b, c] => a + b + c; _ => 0 }").unwrap(),
            Value::Int(6)
        );
        // `...rest` binds the tail as a list.
        assert_eq!(
            run("match [1, 2, 3, 4] { [first, ...rest] => rest.len(); _ => 0 }").unwrap(),
            Value::Int(3)
        );
        // Fixed arity: a length mismatch falls through.
        assert_eq!(
            run(r#"match [1, 2] { [a, b, c] => "three"; [a, b] => "two"; _ => "other" }"#).unwrap(),
            Value::Str("two".into())
        );
        assert_eq!(
            run(r#"match [] { [] => "empty"; _ => "nonempty" }"#).unwrap(),
            Value::Str("empty".into())
        );
        assert_eq!(
            run(r#"match [1] { [] => "empty"; [a] => "one:{a}"; _ => "other" }"#).unwrap(),
            Value::Str("one:1".into())
        );
    }

    #[test]
    fn match_comma_separated_arms_parse() {
        assert_eq!(
            run(r#"match 2 { 1 => "a", 2 => "b", _ => "c" }"#).unwrap(),
            Value::Str("b".into())
        );
    }
}
