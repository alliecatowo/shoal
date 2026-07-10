//! Tree-walk evaluator for shoal's canonical AST.

mod builtins;

use shoal_adapters::{AdapterCatalog, AdapterClass, SubSpec};
use shoal_ast::*;
use shoal_exec::{CancelToken, ExecMode, ExecSpec, StdinSpec};
use shoal_leash::{Effect, Estimates, Plan, Reversibility};
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
    adapters: AdapterCatalog,
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
        }
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
                let vals = self.values_from(iter_value)?;
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
            Expr::DateTime { iso, .. } => parse_datetime(iso).map(|z| Value::DateTime(Box::new(z))),
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
                } else {
                    let args = self.eval_args(args)?;
                    shoal_value::methods::call_method(self, v, name, args, span)
                }
            }
            Expr::FnCall { name, args, .. } => {
                let a = self.eval_args(args)?;
                if let Some(value) = self.call_constructor(name, &a)? {
                    return Ok(value);
                }
                let f = self.env.get(name).ok_or_else(|| {
                    ErrorVal::new("undefined_var", format!("undefined function `{name}`"))
                })?;
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
        if let Some(bound) = self.env.get(&call.head)
            && !call.forced
            && bound.is_callable()
        {
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
        if builtins::is_builtin(&call.head) {
            return builtins::run(self, call);
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
        if call.head == "source" || call.head == "run" || call.head.ends_with(".shl") {
            let is_source = call.head == "source";
            let script_path = if call.head == "source" || call.head == "run" {
                let p = call
                    .args
                    .first()
                    .map(|a| self.cmd_arg_value(a))
                    .transpose()?
                    .ok_or_else(|| ErrorVal::new("arg_error", format!("{} expects script path", call.head)))?;
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
                    argv.push(format!("--{}", name.replace('_', "-")).into());
                    if let Some(value) = value {
                        let v = self.cmd_arg_value(value)?;
                        validate_adapter_value(&v, &param.ty)?;
                        argv.push(self.argv_value(v)?);
                    } else if !param.ty.trim_end_matches('?').eq("bool") {
                        i += 1;
                        let next = call.args.get(i).ok_or_else(|| {
                            ErrorVal::arg_error(format!("--{name} requires a value"))
                        })?;
                        let v = self.cmd_arg_value(next)?;
                        validate_adapter_value(&v, &param.ty)?;
                        argv.push(self.argv_value(v)?);
                    }
                }
                CmdArg::FlagShort { chars, .. } => {
                    for ch in chars.chars() {
                        if !spec.short_flags.contains_key(&ch.to_string()) {
                            return Err(ErrorVal::arg_error(format!(
                                "{}: unknown short flag -{ch}",
                                call.head
                            )));
                        }
                    }
                    argv.push(format!("-{chars}").into());
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
        let value = eval(&program, dir.path()).unwrap();
        assert!(
            matches!(value, Value::Table(rows) if rows.len() == 1 && rows[0]["name"] == Value::Path("a".into()))
        );

        let rm = shoal_syntax::parse("rm a").unwrap();
        let value = eval(&rm, dir.path()).unwrap();
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
}
