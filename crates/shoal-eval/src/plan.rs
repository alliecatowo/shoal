//! Leash plan derivation: a conservative, concrete-effect walk over the AST
//! that never spawns or mutates (docs/CONTRACTS.md leash integration).

use super::*;

impl Evaluator {
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

    pub(crate) fn plan_stmt(
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

    pub(crate) fn plan_block(
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

    pub(crate) fn plan_expr(
        &mut self,
        expr: &Expr,
        functions: &std::collections::HashMap<String, Block>,
        aliases: &std::collections::HashMap<String, CmdCall>,
        out: &mut Vec<Effect>,
        depth: usize,
    ) -> VResult<()> {
        match expr {
            Expr::Cmd { call, .. } => self.plan_call(call, functions, aliases, out, depth),
            Expr::LangBlock { .. } => {
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
            Expr::MethodCall {
                recv, name, args, ..
            } => {
                // `http.get/post/put/delete(url, …)` declares a `net.connect`
                // effect for leash + plan (ROADMAP R2). The host is parsed from a
                // literal URL argument; a non-literal URL declares an
                // unknown-host connect (`*`).
                if let Expr::Var { name: ns, .. } = &**recv
                    && ns == "http"
                    && matches!(name.as_str(), "get" | "post" | "put" | "delete")
                {
                    let (host, port) = args
                        .pos
                        .first()
                        .and_then(url_literal)
                        .map(|u| url_host_port(&u))
                        .unwrap_or_else(|| ("*".into(), 443));
                    push_effect(out, Effect::NetConnect { host, port });
                }
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
            Expr::With {
                cwd,
                env,
                reef,
                body,
                ..
            } => {
                if let Some(e) = cwd {
                    self.plan_expr(e, functions, aliases, out, depth)?
                }
                if let Some(e) = env {
                    self.plan_expr(e, functions, aliases, out, depth)?
                }
                if let Some(e) = reef {
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

    pub(crate) fn plan_call(
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

    pub(crate) fn builtin_effects(&self, call: &CmdCall) -> VResult<Vec<Effect>> {
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
            "ls" | "cat" | "stat" | "head" => vec![Effect::FsRead {
                paths: if ps.is_empty() {
                    vec![self.cwd.clone()]
                } else {
                    ps
                },
            }],
            "mkdir" | "touch" => vec![Effect::FsWrite { paths: ps }],
            "ln" => vec![Effect::FsWrite {
                paths: ps.into_iter().skip(1).collect(),
            }],
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

    pub(crate) fn plan_bindings(
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

/// Push `effect` unless it's already present (small-N linear scan; effect
/// lists are short per plan).
pub(crate) fn push_effect(out: &mut Vec<Effect>, effect: Effect) {
    if !out.contains(&effect) {
        out.push(effect)
    }
}

/// The literal string value of an expression, if it is a plain string literal
/// (for extracting a `net.connect` host from `http.get("https://…")`).
fn url_literal(e: &Expr) -> Option<String> {
    match e {
        Expr::Str { value, .. } => Some(value.clone()),
        _ => None,
    }
}

/// Parse `host` and `port` from a URL for a `net.connect` effect. Defaults to the
/// scheme port (443 for https, 80 otherwise) when the URL has no explicit port.
fn url_host_port(url: &str) -> (String, u16) {
    let default_port = if url.starts_with("https") { 443 } else { 80 };
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let authority = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_scheme);
    // Strip any userinfo (`user:pass@host`).
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    match host_port.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
            (h.to_string(), p.parse().unwrap_or(default_port))
        }
        _ => (host_port.to_string(), default_port),
    }
}

impl Evaluator {
    /// `plan { … }` / `plan <cmd …>` (ROADMAP R3): derive and render the effect
    /// plan without spawning or mutating. The derived program is stashed so a
    /// later `apply <ref>` can run it; the returned record carries its `id`.
    pub(crate) fn builtin_plan(&mut self, call: &CmdCall) -> VResult<Value> {
        let program = self.plan_target_program(call)?;
        let plan = self.plan_program(&program)?;
        self.plans.push(program);
        let id = self.plans.len() as i64;
        Ok(plan_record(&plan, Some(id)))
    }

    /// `apply <ref>` (ROADMAP R3): run a previously-derived `plan { … }`. The ref
    /// is the record `plan` returned (its `id` field) or a bare plan id int.
    pub(crate) fn builtin_apply(&mut self, call: &CmdCall) -> VResult<Value> {
        let vs = self.collect_cmd_values(call)?;
        let id = match vs.first() {
            Some(Value::Int(n)) => *n,
            // A bare `apply 3` word arrives as a str; accept a numeric one.
            Some(Value::Str(s)) if s.parse::<i64>().is_ok() => s.parse().unwrap(),
            Some(Value::Record(r)) => match r.get("id") {
                Some(Value::Int(n)) => *n,
                _ => {
                    return Err(ErrorVal::arg_error(
                        "apply expects a plan reference (a `plan { … }` result or its id)",
                    ));
                }
            },
            _ => {
                return Err(ErrorVal::arg_error(
                    "apply expects a plan reference: `apply <plan>`",
                ));
            }
        };
        let idx = usize::try_from(id - 1)
            .ok()
            .filter(|i| *i < self.plans.len())
            .ok_or_else(|| ErrorVal::new("not_found", format!("no plan #{id} to apply")))?;
        let program = self.plans[idx].clone();
        self.eval_program(&program)
    }

    /// `explain(src)` (ROADMAP R2/R3): parse a source string and render what it
    /// would do — its effect plan — without running it.
    pub(crate) fn builtin_explain(&mut self, call: &CmdCall) -> VResult<Value> {
        let vs = self.collect_cmd_values(call)?;
        let src = match vs.first() {
            Some(Value::Str(s)) => s.clone(),
            Some(Value::Path(p)) => p.to_string_lossy().into_owned(),
            _ => return Err(ErrorVal::arg_error("explain expects a source string")),
        };
        let program =
            shoal_syntax::parse(&src).map_err(|e| ErrorVal::new("parse_error", e.to_string()))?;
        let plan = self.plan_program(&program)?;
        let mut r = match plan_record(&plan, None) {
            Value::Record(r) => r,
            _ => Record::new(),
        };
        r.insert("source".into(), Value::Str(src));
        Ok(Value::Record(r))
    }

    /// The program a `plan`/`apply` verb targets: a trailing `{ … }` block, or a
    /// bare `plan rm x` command reconstructed from the remaining words.
    fn plan_target_program(&self, call: &CmdCall) -> VResult<Program> {
        if let Some(block) = &call.trailing {
            return Ok(Program {
                stmts: block.stmts.clone(),
            });
        }
        let mut args = call.args.iter();
        let head = args.next().and_then(cmd_arg_word).ok_or_else(|| {
            ErrorVal::arg_error("plan expects a block `plan { … }` or a command `plan <cmd> …`")
        })?;
        let inner = CmdCall {
            head,
            forced: false,
            args: args.cloned().collect(),
            redirects: vec![],
            env_prefix: vec![],
            background: false,
            trailing: None,
            span: call.span,
        };
        Ok(Program {
            stmts: vec![Stmt::Expr {
                expr: Expr::Cmd {
                    call: Box::new(inner),
                    span: call.span,
                },
                span: call.span,
            }],
        })
    }
}

/// The literal word text of a command argument (for `plan <cmd> …` head/args).
fn cmd_arg_word(arg: &CmdArg) -> Option<String> {
    match arg {
        CmdArg::Word { text, .. } | CmdArg::Path { text, .. } => Some(text.clone()),
        _ => None,
    }
}

/// Render a derived [`Plan`] as a shoal record: `{id?, effects: [str], reversible,
/// spawns}`. Human-readable and machine-usable (the `id` feeds `apply`).
fn plan_record(plan: &Plan, id: Option<i64>) -> Value {
    let effects: Vec<Value> = plan
        .effects
        .iter()
        .map(|e| Value::Str(effect_str(e)))
        .collect();
    let spawns = plan
        .effects
        .iter()
        .any(|e| matches!(e, Effect::ProcSpawn { .. } | Effect::Opaque));
    let mut r = Record::new();
    if let Some(id) = id {
        r.insert("id".into(), Value::Int(id));
    }
    r.insert("effects".into(), Value::List(effects));
    r.insert(
        "reversible".into(),
        Value::Bool(matches!(plan.reversibility, Reversibility::Reversible)),
    );
    r.insert("spawns".into(), Value::Bool(spawns));
    Value::Record(r)
}

fn join_paths(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// A one-line human description of a single effect, for `plan`/`explain` output.
fn effect_str(e: &Effect) -> String {
    match e {
        Effect::FsRead { paths } => format!("read {}", join_paths(paths)),
        Effect::FsWrite { paths } => format!("write {}", join_paths(paths)),
        Effect::FsDelete { paths } => format!("delete {}", join_paths(paths)),
        Effect::ProcSpawn { argv0, .. } => format!("spawn {argv0}"),
        Effect::NetConnect { host, port } => format!("connect {host}:{port}"),
        Effect::NetListen { port } => format!("listen {port}"),
        Effect::EnvRead { names } => format!("env-read {}", names.join(", ")),
        Effect::EnvWrite { names } => format!("env-write {}", names.join(", ")),
        Effect::SecretUse { names } => format!("secret-use {}", names.join(", ")),
        Effect::SessionWrite => "session-write".to_string(),
        Effect::JournalRead => "journal-read".to_string(),
        Effect::Time => "read-clock".to_string(),
        Effect::Opaque => "opaque (effects unknown)".to_string(),
    }
}
