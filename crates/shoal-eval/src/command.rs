//! Command evaluation: `eval_command`'s big dispatch (session callables,
//! bound-name-as-value, builtin heads, adapters, external spawn + redirects),
//! adapter argv construction, and the exec-spawning core (`run_argv`).

use super::*;
use crate::coerce::{coerce_call_args, signature, validate_adapter_value};
use crate::host::builtin_outcome;

impl Evaluator {
    pub(crate) fn eval_command(&mut self, call: &CmdCall, position: Position) -> VResult<Value> {
        // Trailing `&` desugars to `spawn { <call> }` (site/content/internals/language-conformance-contract.md): the command
        // runs on a background task and the statement yields a `task` handle
        // instead of running synchronously.
        if call.background {
            let mut inner = call.clone();
            inner.background = false;
            let body = Block {
                stmts: vec![Stmt::Expr {
                    expr: Expr::Cmd {
                        call: Box::new(inner),
                        span: call.span,
                    },
                    span: call.span,
                }],
                span: call.span,
            };
            return self.spawn_block(body);
        }
        // Session callables (fns/aliases) resolve as commands even when `^`-forced
        // (defect #3): `^` bypasses only non-callable let/var shadows.
        if let Some(bound) = self.env.get(&call.head)
            && bound.is_callable()
        {
            // `deploy --help` synthesises the signature + doc (site/content/internals/language-conformance-contract.md, defect #12).
            if let Value::Closure(c) = &bound
                && call
                    .args
                    .iter()
                    .any(|a| matches!(a, CmdArg::FlagLong { name, .. } if name == "help"))
            {
                let help = crate::helpers::closure_help(c);
                self.emit(&Value::Str(help));
                return Ok(Value::Null);
            }
            // A parameter typed `glob` owns expansion itself (site/content/internals/language-conformance-contract.md): the
            // callee receives the compiled, unexpanded pattern, so a glob-typed
            // positional slot must skip the generic glob-expansion path below.
            let closure_sig: Option<(&[Param], Option<&RestParam>)> = match &bound {
                Value::Closure(c) => Some((&c.params, c.rest.as_ref())),
                _ => None,
            };
            let mut pos = Vec::new();
            let mut named = Vec::new();
            let mut i = 0;
            while i < call.args.len() {
                let a = &call.args[i];
                match a {
                    CmdArg::FlagLong { name, value, .. } => {
                        let v = match value {
                            Some(v) => self.cmd_arg_value(v)?,
                            // `--name v` ≡ `--name=v` when `name` is a declared
                            // non-bool parameter (site/content/internals/language-conformance-contract.md): the flag consumes
                            // the next word as its value instead of binding
                            // presence and rerouting the word as a positional.
                            // Bool-typed (and untyped/unknown) names keep
                            // presence semantics.
                            None if closure_sig.is_some_and(|(params, _)| {
                                params.iter().any(|p| {
                                    p.name == *name
                                        && p.ty.as_ref().is_some_and(|t| t.name != "bool")
                                })
                            }) =>
                            {
                                i += 1;
                                let next = call.args.get(i).ok_or_else(|| {
                                    ErrorVal::arg_error(format!("--{name} requires a value"))
                                })?;
                                self.cmd_arg_value(next)?
                            }
                            None => Value::Bool(true),
                        };
                        named.push((name.clone(), v));
                    }
                    CmdArg::Glob { .. }
                        if closure_sig.is_some_and(|(params, rest)| {
                            crate::coerce::expected_param_ty(params, rest, pos.len())
                                == Some("glob")
                        }) =>
                    {
                        pos.push(self.cmd_arg_value(a)?);
                    }
                    // A non-variadic `list<T>` param receives an entire word/glob
                    // expansion as one list (site/content/internals/language-conformance-contract.md): `showpaths *.txt` binds
                    // every sorted match to `paths: list<path>`, not just the
                    // first. Element type coercion (`path`/`str`/…) applies per
                    // item; `coerce_call_args` leaves the assembled list intact.
                    CmdArg::Glob { .. } | CmdArg::Word { .. } | CmdArg::Path { .. }
                        if closure_sig
                            .and_then(|(params, _)| params.get(pos.len()))
                            .and_then(|p| p.ty.as_ref())
                            .is_some_and(|t| t.name == "list") =>
                    {
                        let elem = closure_sig
                            .and_then(|(params, _)| params.get(pos.len()))
                            .and_then(|p| p.ty.as_ref())
                            .and_then(|t| t.args.first())
                            .map(|t| t.name.clone())
                            .unwrap_or_else(|| "str".into());
                        let items = self
                            .expand_arg(a)?
                            .into_iter()
                            .map(|v| crate::coerce::coerce_word(v, &elem))
                            .collect::<VResult<Vec<_>>>()?;
                        pos.push(Value::List(items));
                    }
                    _ => pos.extend(self.expand_arg(a)?),
                }
                i += 1;
            }
            // Coerce CMD words to the callee's declared param types (defect #12).
            if let Value::Closure(c) = &bound {
                coerce_call_args(&c.params, c.rest.as_ref(), &mut pos, &mut named)?;
            }
            return self.call_value(&bound, CallArgs { pos, named });
        }
        // A bare word bound to a non-callable value (e.g. `it`, `out`, or any
        // `let`) resolves to that value — bound names dispatch as EXPR (site/content/internals/language-conformance-contract.md).
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
        // `exit [code: int = 0]` / `quit`: request the host to stop. We record
        // the code and let `eval_program` halt its statement loop; the host
        // (REPL / -c / script) honors it via `take_exit`. NEVER process::exit
        // here — that would kill the kernel/embedded host (defect: no exit).
        if call.head == "exit" || call.head == "quit" {
            let code = self.exit_code_arg(call)?;
            self.pending_exit = Some(code);
            return Ok(Value::Null);
        }
        // plan/apply/explain REPL verbs (site/content/internals/roadmap-and-priorities.md). `plan { … }` renders the
        // effect plan without spawning; `apply <ref>` runs a derived plan;
        // `explain(src)` renders what a source string would do. Intercepted before
        // builtin/adapter/external dispatch so `plan`/`apply`/`explain` are verbs.
        if call.head == "plan" {
            return self.builtin_plan(call);
        }
        if call.head == "apply" {
            return self.builtin_apply(call);
        }
        if call.head == "explain" {
            return self.builtin_explain(call);
        }
        if call.head == "interact" {
            return self.builtin_interact(call);
        }
        // `assert(cond, msg?)` (site/content/internals/intercrate-protocol-contracts.md) — also reachable as a command head.
        if call.head == "assert" {
            let pos = self.collect_cmd_values(call)?;
            return self
                .builtin_assert(&CallArgs {
                    pos,
                    named: Vec::new(),
                })
                .map_err(|e| e.or_span(call.span));
        }
        if call.head == "open" {
            let vs = self.collect_cmd_values(call)?;
            return self.builtin_open(vs);
        }
        if call.head == "save" {
            let vs = self.collect_cmd_values(call)?;
            return self.builtin_save(vs);
        }
        // `which` is reef-aware (site/content/internals/reef-resolution.md): it renders a resolution report, not a
        // bare path. Intercepted before the generic builtin dispatch so it can
        // reach the scope chain; still wrapped as an outcome + redirect-capable.
        if call.head == "which" {
            let value = self.builtin_which(call)?;
            let outcome = builtin_outcome("which", value);
            return self.apply_builtin_redirects(call, outcome);
        }
        // `reef` builtin family (site/content/internals/reef-resolution.md): binding table, add, lock, fetch.
        if call.head == "reef" {
            let value = self.builtin_reef(call)?;
            let outcome = builtin_outcome("reef", value);
            return self.apply_builtin_redirects(call, outcome);
        }
        if call.head == "undo" {
            return self.builtin_undo(call);
        }
        if call.head == "journal" || call.head == "history" {
            return self.builtin_journal_view(call);
        }
        if builtins::is_builtin(&call.head) {
            // Outcome unification (P1a): a builtin yields a `Value::Outcome`
            // exactly like an external command — its structured result becomes
            // the outcome's `.out` (`parsed`), `status = 0`/`ok = true`. A
            // builtin error still raises as before (via `?`).
            //
            // site/content/internals/language-conformance-contract.md undo: capture prior state of an overwriting cp/mv/save
            // BEFORE the mutation, then record the typed inverse AFTER. All a
            // no-op unless a journal is installed and a statement is executing.
            let undo_pre = self.fs_undo_pre(&call.head, call);
            let value = builtins::run(self, call)?;
            self.fs_undo_post(&call.head, undo_pre, &value);
            let outcome = builtin_outcome(&call.head, value);
            // Redirects apply to builtin results too (defect #8).
            return self.apply_builtin_redirects(call, outcome);
        }
        if call.head == "cd" {
            return self.eval_cd(call);
        }
        // `pushd`/`popd`/`dirs`: the bash directory stack (session-scoped). Like
        // `cd`, they mutate the session cwd, so they are intercepted here rather
        // than in the pure `builtins::run` dispatch (which cannot change cwd).
        if call.head == "pushd" {
            return self.eval_pushd(call);
        }
        if call.head == "popd" {
            return self.eval_popd(call);
        }
        if call.head == "dirs" {
            return self.eval_dirs(call);
        }
        // `j`/`jump`: frecency-ranked directory jump (frecency.rs). A session-
        // cwd mutation like `cd`, so it is intercepted here rather than in the
        // pure `builtins::run` dispatch (which cannot change the cwd).
        if call.head == "j" || call.head == "jump" {
            return self.eval_jump(call);
        }
        if call.head == "pwd" {
            return Ok(Value::Path(self.cwd.clone()));
        }
        // `run` is the poly runner + dynamic form (site/content/internals/pty-job-control.md): dispatch by extension
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
            if is_source {
                let src =
                    self.host.fs.read_to_string(&path).map_err(|e| {
                        ErrorVal::new("io_error", format!("cannot read script: {e}"))
                    })?;
                let program = shoal_syntax::parse(&src)
                    .map_err(|e| ErrorVal::new("parse_error", e.to_string()))?;
                return self.eval_program(&program);
            }
            // A `.shl` head runs as a separate program in a child evaluator
            // with a fresh lexical scope (see
            // `site/content/internals/values-streams-execution.md`) — share the
            // `run x.shl` path so bindings cannot leak into this session.
            let args = self.collect_cmd_values(call)?;
            return self.run_script_file(&path, Some("shl"), args, position);
        }
        // `^name` bypasses adapters too (language card): the forced head must
        // reach the real command, not the adapter's flag/signature gate.
        if !call.forced && self.host.adapters.lookup(&call.head).is_some() {
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
        let fs = self.host.fs.clone();
        for r in &call.redirects {
            let target = self.arg_path(&r.target)?;
            match r.kind {
                // Undo (site/content/internals/language-conformance-contract.md): an external command's `> file` / `>> file`
                // clobbers the target's contents just like `cp` — snapshot the
                // prior bytes first, record the restore inverse after, so
                // `some-cmd > f` and `sh { … } > f` are reversible too.
                RedirectKind::Out => {
                    let undo_pre = self.redirect_undo_pre(&target);
                    // site/content/internals/language-conformance-contract.md: write the FULL stdout (load from CAS when it
                    // spilled), never just the resident preview.
                    fs.write(&target, &out.stdout_bytes()?)
                        .map_err(|e| ErrorVal::new("custom", e.to_string()))?;
                    self.overwrite_undo_post(undo_pre);
                }
                RedirectKind::Append => {
                    let undo_pre = self.redirect_undo_pre(&target);
                    fs.append(&target, &out.stdout_bytes()?)
                        .map_err(|e| ErrorVal::new("custom", e.to_string()))?;
                    self.overwrite_undo_post(undo_pre);
                }
                RedirectKind::In => {}
            }
        }
        Ok(value)
    }

    /// The single choke point for a real session cwd change (`cd`, `cd -`,
    /// `pushd`, `popd`, `j`): stash the prior cwd as OLDPWD (so `cd -` returns
    /// to the *exact* directory left, byte-identical), move to `new`, and feed
    /// the destination to the `j`/`jump` frecency store (best-effort — a store
    /// write failure never fails the navigation). `with cwd:` and module loads
    /// deliberately do NOT flow through here: those are scoped save/restore cwd
    /// swaps, not navigation the user asked the shell to remember.
    pub(crate) fn change_cwd(&mut self, new: PathBuf) {
        let prev = std::mem::replace(&mut self.cwd, new);
        self.oldpwd = Some(prev);
        let cwd = self.cwd.clone();
        self.record_cd(&cwd);
    }

    /// Reject a session-cwd mutation (`cd`/`pushd`/`popd`) inside a `fn` body
    /// (site/content/internals/language-conformance-contract.md): a fn must not move the ambient session cwd — `with cwd:` is
    /// the scoped alternative. A pure guard shared by all three verbs.
    fn ensure_cwd_mutable(&self, verb: &str, span: Span) -> VResult<()> {
        if self.in_fn_body > 0 {
            return Err(ErrorVal::new(
                "custom",
                format!(
                    "{verb} is only allowed at session top level; use `with cwd:` inside a fn body"
                ),
            )
            .with_span(span));
        }
        Ok(())
    }

    /// `cd [dir]` / `cd -` (site/content/internals/language-conformance-contract.md). Bare `cd` goes to `$HOME`; `cd -` returns
    /// to the previous directory (OLDPWD) and echoes it (bash parity, achieved
    /// by returning the `Path`, which the statement sink renders); otherwise cd
    /// to the resolved, canonicalized path. Every form records into the frecency
    /// store and updates OLDPWD via [`Evaluator::change_cwd`].
    fn eval_cd(&mut self, call: &CmdCall) -> VResult<Value> {
        self.ensure_cwd_mutable("cd", call.span)?;
        // `cd -`: jump back to the previous directory (bash's `$OLDPWD`).
        if matches!(call.args.first(), Some(CmdArg::Dash { .. })) {
            let Some(prev) = self.oldpwd.clone() else {
                return Err(ErrorVal::new("custom", "cd: OLDPWD not set").with_span(call.span));
            };
            self.change_cwd(prev);
            return Ok(Value::Path(self.cwd.clone()));
        }
        let target = self.cd_target(call)?;
        self.change_cwd(target);
        Ok(Value::Path(self.cwd.clone()))
    }

    /// Resolve a `cd`/`pushd` path argument to an absolute, canonicalized
    /// directory. A missing argument means `$HOME` (the bare-`cd` case; `pushd`
    /// never calls this with no argument — that is its swap form). A non-path
    /// value is an `arg_error`; a path that does not resolve is one too.
    fn cd_target(&mut self, call: &CmdCall) -> VResult<PathBuf> {
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
        let joined = if p.is_absolute() { p } else { self.cwd.join(p) };
        joined
            .canonicalize()
            .map_err(|e| ErrorVal::new("arg_error", e.to_string()))
    }

    /// `pushd [dir]` — the bash directory stack. With a `dir`: push the current
    /// cwd onto the stack and cd into `dir`. With no argument: swap the current
    /// cwd with the most-recent stacked directory (an error when the stack is
    /// empty). Returns the new stack, exactly as `dirs` renders it.
    fn eval_pushd(&mut self, call: &CmdCall) -> VResult<Value> {
        self.ensure_cwd_mutable("pushd", call.span)?;
        if call.args.is_empty() {
            let Some(top) = self.dir_stack.first().cloned() else {
                return Err(ErrorVal::new(
                    "custom",
                    "pushd: no other directory on the stack to swap with",
                )
                .with_span(call.span));
            };
            // Swap: the current cwd takes the top slot, we move to the old top.
            self.dir_stack[0] = self.cwd.clone();
            self.change_cwd(top);
            return Ok(self.dir_stack_value());
        }
        let target = self.cd_target(call)?;
        self.dir_stack.insert(0, self.cwd.clone());
        self.change_cwd(target);
        Ok(self.dir_stack_value())
    }

    /// `popd` — pop the most-recent stacked directory and cd into it. An empty
    /// stack is an error (nothing to pop). Returns the remaining stack.
    fn eval_popd(&mut self, call: &CmdCall) -> VResult<Value> {
        self.ensure_cwd_mutable("popd", call.span)?;
        if self.dir_stack.is_empty() {
            return Err(
                ErrorVal::new("custom", "popd: directory stack is empty").with_span(call.span)
            );
        }
        let target = self.dir_stack.remove(0);
        self.change_cwd(target);
        Ok(self.dir_stack_value())
    }

    /// `dirs` — the directory stack as a typed `list<path>`, current directory
    /// first (`[cwd] ++ dir_stack`). Structured, not text, so it dot-chains:
    /// `dirs.len()`, `dirs.first()`, `dirs.where(...)`.
    fn eval_dirs(&mut self, _call: &CmdCall) -> VResult<Value> {
        Ok(self.dir_stack_value())
    }

    /// Build the shared `dirs`/`pushd`/`popd` return value: `[cwd] ++ dir_stack`
    /// as a `list<path>`, current directory first (bash's left-to-right order).
    fn dir_stack_value(&self) -> Value {
        let mut out = Vec::with_capacity(self.dir_stack.len() + 1);
        out.push(Value::Path(self.cwd.clone()));
        out.extend(self.dir_stack.iter().cloned().map(Value::Path));
        Value::List(out)
    }

    pub(crate) fn eval_adapter(&mut self, call: &CmdCall, position: Position) -> VResult<Value> {
        let adapter = self
            .host
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
                        // Single-character params emit the POSIX single-dash
                        // form: git has `-n`, not `--n` — this used to
                        // validate `--n` and forward it verbatim, which git
                        // rejects ("ambiguous argument"), leaving the
                        // adapter's own advertised flag unusable.
                        let spelled = if name.chars().count() == 1 {
                            format!("-{name}")
                        } else {
                            format!("--{}", name.replace('_', "-"))
                        };
                        argv.push(spelled.into());
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

    pub(crate) fn run_argv(
        &mut self,
        mut argv: Vec<OsString>,
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
        // reef spawn-time resolution (site/content/internals/reef-resolution.md). A pure no-op unless
        // the head is a bare name constrained by a manifest in scope — so a
        // repo with no `.reef.toml` spawns exactly as before. When reef resolves
        // the head it hands back the binary's content hash so the leash spawn
        // gate below can reuse it rather than re-hashing the same file.
        let reef_hash = self.reef_apply(&mut argv, &mut env, span)?;
        let force_tui = meta.as_ref().is_some_and(|m| m.class == AdapterClass::Tui);
        let mode = if force_tui || (self.session.interactive && position == Position::Statement) {
            ExecMode::PtyTee
        } else {
            ExecMode::Capture
        };
        // A PTY child owns the real terminal for its run (site/content/internals/language-conformance-contract.md "byte-
        // identical to bash"): unless a redirect (`< file`) or `.feed` already
        // claimed stdin, forward the user's tty — shoal-exec then engages raw
        // mode on the real terminal and pumps stdin/resizes to the child.
        // Without this, interactive TUIs (vim, claude, htop) get output-only
        // PTYs: the cooked-mode line discipline echoes every mouse event and
        // terminal query response as `^[[…` caret junk and delivers keystrokes
        // only on Enter.
        let stdin = if mode == ExecMode::PtyTee && matches!(stdin, StdinSpec::Null) {
            StdinSpec::Inherit
        } else {
            stdin
        };
        // Only the PtyTee path streams the child's bytes to the real terminal;
        // the result renderer suppresses re-rendering exactly these (defect #1).
        let streamed = mode == ExecMode::PtyTee;
        let display = argv
            .iter()
            .map(|x| x.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        // site/content/internals/language-conformance-contract.md leash activation: under a scoped leash policy, wrap the child
        // in the strongest available OS backend (Landlock/Seatbelt) before
        // exec. `resolve_sandbox` returns `None` for the default-permissive
        // policy (and when no policy is installed), so this is a pure no-op on
        // the normal path — the child spawns exactly as before.
        let sandbox = self.resolve_sandbox();
        // site/content/internals/language-conformance-contract.md spawn-hash pinning: consult the leash effect evaluator with
        // this spawn's *resolved* binary (post-reef `argv[0]`) before exec. A
        // pure no-op unless the active principal pins `proc_spawn` — see
        // `spawn_gate`, which guards against a default-deny regression by only
        // hashing/evaluating when a non-empty allowlist is actually configured.
        if let Some(argv0) = argv.first() {
            self.spawn_gate(argv0, reef_hash.as_deref(), span)?;
        }
        // Capture the head before `argv` moves into the spawn spec, so a
        // `not_found` failure below can offer a command did-you-mean (site/content/internals/language-conformance-contract.md).
        let failed_head = argv.first().map(|s| s.to_string_lossy().into_owned());
        // site/content/internals/language-conformance-contract.md disk-spill: for value-position captures only (never PTY,
        // whose output already reached the terminal), and only when a journal
        // is installed to adopt the overflow into its CAS. `None` otherwise
        // preserves the exact pre-spill behavior (bounded RAM buffer, overflow
        // dropped) — so `-c`/scripts/conformance are wholly untouched.
        let spill = if mode == ExecMode::Capture {
            self.session
                .journal
                .as_ref()
                .and_then(|j| j.spill_dir().ok())
                .map(|dir| shoal_exec::SpillConfig { dir })
        } else {
            None
        };
        // Spawn through the Exec port (site/content/internals/roadmap-and-priorities.md). The default
        // `StdExec` is `shoal_exec::run` verbatim, so this is byte-identical.
        let exec = self.host.exec.clone();
        let mut r = exec
            .run(
                ExecSpec {
                    argv,
                    cwd: self.cwd.clone(),
                    env,
                    stdin,
                    mode,
                    sandbox,
                    spill,
                },
                &self.cancel,
            )
            .map_err(|e| {
                let is_not_found = e.kind() == std::io::ErrorKind::NotFound;
                let err = ErrorVal::new(
                    if is_not_found { "not_found" } else { "custom" },
                    e.to_string(),
                )
                .with_span(span);
                // site/content/internals/language-conformance-contract.md: when the head simply isn't a resolvable command,
                // point at the closest known one (builtins ∪ adapter heads ∪
                // in-scope callable bindings) — mirrors the method did-you-mean.
                // The primary `not_found`/"command not found" code+message is
                // unchanged; we only ADD a hint when a near-miss exists.
                match failed_head
                    .as_deref()
                    .filter(|_| is_not_found)
                    .and_then(|h| self.command_suggestion(h))
                {
                    Some(hint) => err.with_hint(hint),
                    None => err,
                }
            })?;
        // Job control (site/content/internals/language-conformance-contract.md): a foreground PtyTee child that was *stopped*
        // (Ctrl-Z → SIGTSTP) rather than finishing. Register it as a suspended
        // job (so it lists in `jobs` and the REPL `fg`/`bg` can resume its parked
        // PTY by pid) and return a streamed outcome that renders nothing — the
        // REPL sees the pending stop and returns to the prompt. Never raise a
        // `cmd_failed` for a stop: the command did not fail, it is suspended.
        if r.stopped {
            self.register_stopped_external(r.pid, r.pgid as i32, display.clone());
            return Ok(Value::Outcome(Arc::new(OutcomeVal {
                status: None,
                signal: None,
                ok: false,
                stdout: Arc::new(r.stdout),
                // A stopped PtyTee job never spills to CAS (capture spill is a
                // Capture-mode, value-position concern).
                stdout_ref: None,
                stderr: Arc::new(Vec::new()),
                dur_ns: r.dur.as_nanos().min(i64::MAX as u128) as i64,
                pid: r.pid,
                cmd: display,
                parsed: None,
                // The child's bytes already reached the real terminal via the
                // PtyTee passthrough, so the result renderer must not reprint.
                streamed: true,
                span: Some(span),
            })));
        }
        let ok_codes = meta.as_ref().map_or(&[0][..], |m| m.ok_codes.as_slice());
        let ok = r.status.is_some_and(|code| ok_codes.contains(&code));
        let parsed = meta.as_ref().and_then(|m| {
            shoal_adapters::parse_output(&m.parse, &r.stdout, m.output_type.as_deref())
        });
        // Take the resident stdout once (it is the bounded preview when a spill
        // occurred, the whole thing otherwise) and share the one allocation
        // between the outcome's `.stdout` and any site/content/internals/language-conformance-contract.md ref-backed view.
        let stdout = Arc::new(std::mem::take(&mut r.stdout));
        // site/content/internals/language-conformance-contract.md: if stdout overflowed the RAM cap and spilled to disk, adopt
        // the spill into the CAS and back `.stdout` with a lazy ref (true
        // length + on-demand load). `None` on every ordinary capture.
        let stdout_ref = r
            .stdout_spill
            .take()
            .and_then(|spill| self.adopt_capture_spill(&spill, stdout.clone()));
        let out = Value::Outcome(Arc::new(OutcomeVal {
            status: r.status,
            signal: r.signal,
            ok,
            stdout,
            stdout_ref,
            stderr: Arc::new(r.stderr),
            dur_ns: r.dur.as_nanos().min(i64::MAX as u128) as i64,
            pid: r.pid,
            cmd: display,
            parsed,
            streamed,
            // Stamp the invocation's source span — the same `span` the sibling
            // error path below hands to `ErrorVal::with_span`, so a command's
            // success and failure carry an identical source anchor on the wire
            // (site/content/internals/kernel-protocol.md).
            span: Some(span),
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

    /// Adopt a value-position capture's disk spill (site/content/internals/language-conformance-contract.md) into the journal
    /// CAS and hand back a lazy, ref-backed view of the full stdout. `preview`
    /// is the bounded resident prefix (shared with the outcome's `.stdout`).
    ///
    /// Returns `None` — degrading to the resident preview — only if there is no
    /// journal (so the spill can never have been produced in the first place)
    /// or adoption fails on I/O; the orphaned spill file is cleaned up in that
    /// case. On success the blob is durable under its real blake3 and pinned so
    /// GC keeps it while the value is live.
    fn adopt_capture_spill(
        &self,
        spill: &shoal_exec::CaptureSpill,
        preview: Arc<Vec<u8>>,
    ) -> Option<Arc<shoal_value::CasBytesVal>> {
        let journal = self.session.journal.as_ref()?;
        if journal
            .ingest_spill(&spill.path, &spill.hash, spill.len, true)
            .is_err()
        {
            let _ = self.host.fs.remove_file(&spill.path);
            return None;
        }
        let loader = CasBytesLoader {
            cas: journal.cas(),
            hash: spill.hash.clone(),
        };
        Some(Arc::new(shoal_value::CasBytesVal {
            hash: spill.hash.clone(),
            len: spill.len,
            preview,
            truncated: spill.truncated,
            loader: Arc::new(loader),
        }))
    }

    /// Resolve a `val:blake3:<hash>` content short-ref (the recoverable form
    /// [`shoal_value::CasBytesVal::reference`] / `.ref` yields) into a lazy
    /// [`Value::CasBytes`] backed by this session's journal CAS, so a bare ref
    /// *written as a value* dispatches methods and materializes exactly like the
    /// spill it came from (this is the in-language mirror of the wire
    /// `value.get` resolution).
    ///
    /// Returns `None` when `s` is not a content ref at all — the caller then
    /// dispatches the string through the ordinary string-method path unchanged.
    /// Returns `Some(Err(..))` — a clear `not_found` — when the ref is genuine
    /// but cannot be resolved: no journal/CAS is installed in this session, or
    /// no blob is tracked under that hash.
    pub(crate) fn resolve_content_ref(&self, s: &str, span: Span) -> Option<VResult<Value>> {
        let hash = shoal_value::CasBytesVal::parse_ref(s)?;
        Some(self.load_content_ref(hash).map_err(|e| e.with_span(span)))
    }

    /// The fallible core of [`Self::resolve_content_ref`]: builds the lazy
    /// [`Value::CasBytes`] for `hash`. `.len` is answered from the `blob` table
    /// metadata alone (never loading the content); a bare ref carries no resident
    /// preview, so `render` shows the ref + true length and materialization loads
    /// on demand through the same [`CasBytesLoader`]/[`shoal_journal::Cas`] seam a
    /// fresh spill uses.
    fn load_content_ref(&self, hash: &str) -> VResult<Value> {
        let prefix = shoal_value::CasBytesVal::REF_PREFIX;
        let Some(journal) = self.session.journal.as_ref() else {
            return Err(ErrorVal::new(
                "not_found",
                format!(
                    "cannot resolve content ref {prefix}{hash}: this session has no journal/CAS"
                ),
            ));
        };
        let len = journal
            .blob_len(hash)
            .map_err(|e| {
                ErrorVal::new(
                    "not_found",
                    format!("cannot resolve content ref {prefix}{hash}: {e}"),
                )
            })?
            .ok_or_else(|| {
                ErrorVal::new(
                    "not_found",
                    format!("no CAS blob for content ref {prefix}{hash}"),
                )
            })?;
        let loader = CasBytesLoader::new(journal.cas(), hash.to_string());
        Ok(Value::CasBytes(Arc::new(shoal_value::CasBytesVal {
            hash: hash.to_string(),
            len,
            preview: Arc::new(Vec::new()),
            truncated: false,
            loader: Arc::new(loader),
        })))
    }

    /// The leash spawn gate (site/content/internals/language-conformance-contract.md content-hash pinning). Consulted from
    /// `run_argv` for every external spawn, just before exec. Returns `Ok(())`
    /// — allow — in every case EXCEPT when the active principal pins process
    /// spawns (a non-empty `proc_spawn` allowlist) AND the resolved binary
    /// matches none of those pins by content hash or name.
    ///
    /// Zero-regression guarantee: when no leash policy is installed, or the
    /// principal declares no `proc_spawn` grants, this returns immediately —
    /// the binary is never hashed and the spawn proceeds exactly as it does
    /// today. It deliberately gates on [`shoal_leash::Policy::spawn_pinning_active`]
    /// rather than calling `evaluate_effect` unconditionally, because an empty
    /// allowlist evaluates a `ProcSpawn` as `Deny` — consulting the evaluator
    /// without that guard would default-deny ordinary commands.
    ///
    /// `reef_hash` is the content hash reef already computed for a resolved
    /// binary (reused verbatim); when `None`, and only when pinning is active,
    /// the resolved binary's own bytes are hashed here.
    pub(crate) fn spawn_gate(
        &self,
        argv0: &OsStr,
        reef_hash: Option<&str>,
        span: Span,
    ) -> VResult<()> {
        let Some((policy, principal)) = self.session.leash.as_ref() else {
            return Ok(());
        };
        // Empty/absent `proc_spawn` grants ⇒ allow, exactly as before pinning
        // existed. This guard is load-bearing: without it, `evaluate_effect`
        // below would deny every spawn under an otherwise-unrestricted policy.
        if !policy.spawn_pinning_active(principal) {
            return Ok(());
        }
        // Reuse reef's hash when it resolved `argv[0]`; otherwise hash the
        // resolved binary's bytes (same blake3-hex as reef and
        // `shoal_leash::preflight_spawn`). An unlocatable/unreadable binary
        // yields an empty hash, falling back to name-only matching — still
        // enforced, never silently allowed.
        let bin_hash = match reef_hash {
            Some(h) => h.to_string(),
            None => self.hash_resolved_bin(argv0).unwrap_or_default(),
        };
        let effect = Effect::ProcSpawn {
            bin_hash,
            argv0: argv0.to_string_lossy().into_owned(),
        };
        match policy.evaluate_effect(principal, &effect) {
            shoal_leash::Verdict::Allow => Ok(()),
            _ => Err(ErrorVal::new(
                "spawn_denied",
                format!(
                    "leash: spawn of `{}` denied — its content hash/name is not in principal `{principal}`'s proc_spawn allowlist",
                    argv0.to_string_lossy()
                ),
            )
            .with_span(span)),
        }
    }

    /// Content-hash the binary `argv0` resolves to — an absolute path as-is, or
    /// a bare name via the ambient `$PATH` (`which`) — returning reef/leash's
    /// blake3-hex so a pin copied from `reef`/`which` output compares equal.
    /// `None` when the binary can't be located or read. Reads through the `Fs`
    /// port so it stays testable without touching a real binary.
    pub(crate) fn hash_resolved_bin(&self, argv0: &OsStr) -> Option<String> {
        let candidate = Path::new(argv0);
        let resolved = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.ambient_which(&argv0.to_string_lossy())?
        };
        let bytes = self.host.fs.read(&resolved).ok()?;
        Some(shoal_reef::hashcache::hash_bytes(&bytes))
    }

    /// True when `name` resolves as a command (builtin, special head, adapter,
    /// or an executable on `PATH`) — drives command-in-expression (defect #5).
    pub(crate) fn is_command_name(&self, name: &str) -> bool {
        // Builtin command heads come straight from the canonical registry
        // (`shoal_syntax::commands`, re-exported through `builtins`): structured
        // builtins via `is_builtin`, the heads special-cased in `eval_command`
        // via `is_special_head`. Deriving both sides from the same data is what
        // keeps this in step with dispatch.
        if builtins::is_builtin(name) || builtins::is_special_head(name) {
            return true;
        }
        if name.contains('/') || name.contains('.') {
            return false;
        }
        if self.host.adapters.lookup(name).is_some() {
            return true;
        }
        let path = self
            .process_env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.as_os_str());
        shoal_exec::which(OsStr::new(name), path).is_some()
    }

    /// Command did-you-mean (site/content/internals/language-conformance-contract.md): when a command head fails to resolve,
    /// find the closest *known* command name so the `not_found` error can carry
    /// a `did you mean 'X'?` hint — the command-head analogue of the method
    /// did-you-mean (`shoal_value::methods::suggest`).
    ///
    /// The candidate vocabulary is deliberately host-INDEPENDENT so the hint is
    /// deterministic and testable: the canonical builtin registry
    /// (`shoal_syntax::commands::builtin_names`), the adapter command heads the
    /// evaluator holds, and the in-scope callable session bindings (fn/alias
    /// names). We do NOT scan `$PATH` — that would be noisy and non-reproducible.
    ///
    /// Threshold mirrors the method hint: names of ≥ 5 chars tolerate an edit
    /// distance of 2, shorter names only 1 (at distance 2 a 4-char typo matches
    /// half the table), and the match must be strictly closer than the typo's
    /// own length so a short head can't match unrelated noise.
    fn command_suggestion(&self, head: &str) -> Option<String> {
        // A reef-rewritten `argv[0]` can be an absolute path, but the user typed
        // a bare name — compare against the final path component.
        let head = head
            .rsplit(['/', std::path::MAIN_SEPARATOR])
            .next()
            .unwrap_or(head);
        let len = head.chars().count();
        if len == 0 {
            return None;
        }
        let max_d = if len >= 5 { 2 } else { 1 };
        // Union the deterministic candidate sources, then sort+dedup so ties
        // break identically every run (the first minimum wins in `min_by_key`).
        let mut candidates: Vec<String> = builtins::builtin_names()
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        candidates.extend(self.host.adapters.names().map(str::to_owned));
        for name in self.env.visible_names() {
            if self.env.get(&name).is_some_and(|v| v.is_callable()) {
                candidates.push(name);
            }
        }
        candidates.sort_unstable();
        candidates.dedup();
        let (dist, best) = candidates
            .iter()
            .map(|c| (shoal_value::methods::levenshtein(head, c), c))
            .min_by_key(|(d, _)| *d)?;
        (dist <= max_d && dist < len).then(|| format!("did you mean '{best}'?"))
    }

    /// Resolve the optional `exit`/`quit` status argument to an `i32`
    /// (default `0`). Accepts a bare integer word (`exit 3`) or an int-valued
    /// expression; anything non-integer is an `arg_error`.
    fn exit_code_arg(&mut self, call: &CmdCall) -> VResult<i32> {
        let vs = self.collect_cmd_values(call)?;
        let Some(first) = vs.into_iter().next() else {
            return Ok(0);
        };
        let code = match crate::coerce::coerce_word(first, "int")? {
            Value::Int(n) => n,
            other => {
                return Err(ErrorVal::arg_error(format!(
                    "exit expects an int status, found {}",
                    other.type_name()
                ))
                .with_span(call.span));
            }
        };
        Ok(code.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32)
    }

    /// Collect a command's positional (non-flag) argument values.
    pub(crate) fn collect_cmd_values(&mut self, call: &CmdCall) -> VResult<Vec<Value>> {
        let mut vs = Vec::new();
        for a in &call.args {
            match a {
                CmdArg::FlagLong { .. } | CmdArg::FlagShort { .. } | CmdArg::DashDash { .. } => {}
                _ => vs.extend(self.expand_arg(a)?),
            }
        }
        Ok(vs)
    }
}

/// Loads a spilled capture's full bytes from the journal CAS on demand.
/// Holds a DB-independent [`shoal_journal::Cas`] (just a path), so a ref-backed
/// [`shoal_value::CasBytesVal`] stays `Send + Sync` and outlives the borrow of
/// the evaluator that produced it.
///
/// Reused verbatim by [`crate::Evaluator::resolve_content_ref`] to back a bare
/// `val:blake3:<hash>` ref written as a value (see
/// `site/content/internals/persistence.md`) — same CAS seam as a fresh spill,
/// so a recovered ref materializes
/// exactly like the capture it came from.
pub(crate) struct CasBytesLoader {
    cas: shoal_journal::Cas,
    hash: String,
}

impl CasBytesLoader {
    pub(crate) fn new(cas: shoal_journal::Cas, hash: String) -> Self {
        Self { cas, hash }
    }
}

impl shoal_value::BytesLoad for CasBytesLoader {
    fn load(&self) -> std::io::Result<Vec<u8>> {
        self.cas.read(&self.hash)
    }
}

#[cfg(test)]
mod command_did_you_mean_tests {
    //! site/content/internals/language-conformance-contract.md command did-you-mean. The conformance corpus
    //! (`spec/cases/edges.toml`) proves the hint reaches the wire for builtin +
    //! session-binding sources; these unit tests pin what the corpus harness
    //! cannot — the adapter candidate source (the corpus evaluator loads none),
    //! the edit-distance threshold, and the precise ABSENCE of a hint for a
    //! too-far head.
    use super::*;

    // `command_suggestion` reads only the candidate vocabulary (builtins,
    // adapters, env) — never the filesystem — so the cwd need not exist.
    fn ev() -> Evaluator {
        Evaluator::new(std::env::temp_dir())
    }

    #[test]
    fn builtin_typos_suggest_the_canonical_head() {
        let ev = ev();
        for (typo, want) in [
            ("journl", "journal"),
            ("puhd", "pushd"),
            ("wich", "which"),
            ("slee", "sleep"),
            ("popdd", "popd"),
            ("historyy", "history"),
        ] {
            assert_eq!(
                ev.command_suggestion(typo).as_deref(),
                Some(format!("did you mean '{want}'?").as_str()),
                "`{typo}` should suggest `{want}`"
            );
        }
    }

    #[test]
    fn a_head_too_far_from_anything_known_gets_no_hint() {
        let ev = ev();
        // `xyzzy` (5 chars, so distance 2 is tolerated) is still nowhere near
        // any builtin; a wildly-unlike head likewise stays hint-free.
        assert_eq!(ev.command_suggestion("xyzzy"), None);
        assert_eq!(ev.command_suggestion("zzzznotarealcommandxyz123"), None);
        // Empty head (a reef basename edge) never suggests.
        assert_eq!(ev.command_suggestion(""), None);
    }

    #[test]
    fn short_heads_only_tolerate_distance_one() {
        let ev = ev();
        // A 2-char head at distance 2 from every 2-char builtin (`ls`/`cp`/…)
        // must NOT match — at distance 2 a short name matches half the table.
        assert_eq!(ev.command_suggestion("qz"), None);
        // …but a genuine distance-1 short typo does resolve.
        assert_eq!(
            ev.command_suggestion("lst").as_deref(),
            Some("did you mean 'ls'?")
        );
    }

    #[test]
    fn adapter_command_heads_join_the_candidate_set() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("pack.toml"),
            "[cmd.docker]\nbin = \"docker\"\n[cmd.kubectl]\nbin = \"kubectl\"\n",
        )
        .unwrap();
        let (catalog, warnings) = shoal_adapters::AdapterCatalog::load_dir(dir.path());
        assert!(warnings.is_empty(), "adapter fixture should parse cleanly");
        let mut ev = Evaluator::new(dir.path().to_path_buf());
        ev.set_adapters(catalog);
        // `dcoker` is a transposition of `docker` (distance 2, len ≥ 5).
        assert_eq!(
            ev.command_suggestion("dcoker").as_deref(),
            Some("did you mean 'docker'?")
        );
        assert_eq!(
            ev.command_suggestion("kubctl").as_deref(),
            Some("did you mean 'kubectl'?")
        );
    }

    #[test]
    fn in_scope_callable_bindings_join_the_candidate_set() {
        let mut ev = ev();
        let program = shoal_syntax::parse("fn deploy() { 42 }").unwrap();
        ev.eval_program(&program).unwrap();
        assert_eq!(
            ev.command_suggestion("deployy").as_deref(),
            Some("did you mean 'deploy'?")
        );
        // A plain (non-callable) `let` binding is NOT a command candidate.
        let program = shoal_syntax::parse("let treasure = 1").unwrap();
        ev.eval_program(&program).unwrap();
        assert_eq!(ev.command_suggestion("treasuer"), None);
    }

    #[test]
    fn a_reef_rewritten_absolute_argv0_matches_on_its_basename() {
        let ev = ev();
        // `argv[0]` can be an absolute path post-reef; the user typed a bare
        // name, so we compare against the final component.
        assert_eq!(
            ev.command_suggestion("/usr/bin/journl").as_deref(),
            Some("did you mean 'journal'?")
        );
    }
}

#[cfg(test)]
mod dispatch_registry_lockstep {
    //! `eval_command`'s special-head dispatch is a hand-written
    //! if-chain of head-equality guards, while the *canonical* builtin registry
    //! (`shoal_syntax::commands`, which the completer/highlighter/LSP consume)
    //! lives elsewhere. A comment asks to "keep this in lockstep" but nothing
    //! enforced it: add a guard here and forget the registry, and the head
    //! dispatches yet is invisible to completion/highlight/LSP.
    //!
    //! This test closes that gap by reading the guards straight out of this
    //! file's own source (`include_str!`), so it can never drift from a
    //! hand-maintained duplicate list — a new guard is picked up automatically.
    use super::*;
    use std::collections::BTreeSet;

    /// Every head literal the production dispatch matches on, extracted from
    /// this file's source. We embed the whole file and cut it at the first
    /// `#[cfg(test)]` so ONLY production `eval_command` code is scanned — the
    /// test modules (including this one) are excluded, which also sidesteps any
    /// self-match on the scanner's own needle.
    fn dispatched_heads() -> BTreeSet<String> {
        const SRC: &str = include_str!("command.rs");
        let production = SRC.split("#[cfg(test)]").next().unwrap();
        let needle = "call.head == \"";
        let mut heads = BTreeSet::new();
        let mut rest = production;
        while let Some(i) = rest.find(needle) {
            rest = &rest[i + needle.len()..];
            if let Some(end) = rest.find('"') {
                heads.insert(rest[..end].to_string());
                rest = &rest[end + 1..];
            }
        }
        heads
    }

    /// Forward: every head the dispatch intercepts is known to the canonical
    /// registry — structured builtin (`is_builtin`, e.g. `which`) or special
    /// head (`is_special_head`). A guard added here without a registry entry
    /// fails this, so it can never become invisible to completion/highlight/LSP.
    #[test]
    fn every_dispatch_guard_is_in_the_registry() {
        let heads = dispatched_heads();
        // Sanity: the scan actually found the guards (guards against a silent
        // regex/split breakage masking real drift).
        assert!(
            heads.len() >= 20,
            "scan found only {} dispatch heads — did command.rs's guard shape change? {heads:?}",
            heads.len()
        );
        for head in &heads {
            assert!(
                builtins::is_builtin(head) || builtins::is_special_head(head),
                "dispatch guard `call.head == \"{head}\"` in command.rs is NOT in the canonical \
                 registry (shoal_syntax::commands) — it dispatches but is invisible to \
                 completion/highlight/LSP. Add `{head}` to NAMES or SPECIAL_HEADS."
            );
        }
    }

    /// Reverse: every SPECIAL head in the registry has a real dispatch guard —
    /// so a registry entry can't advertise a head that `eval_command` never
    /// actually intercepts. (`is_builtin` names route through the generic
    /// `builtins::run` path, not a per-head guard, so they're excluded here.)
    #[test]
    fn every_special_head_in_the_registry_is_dispatched() {
        let heads = dispatched_heads();
        for name in builtins::builtin_names() {
            if builtins::is_special_head(name) {
                assert!(
                    heads.contains(*name),
                    "registry special head `{name}` has no `call.head == \"{name}\"` dispatch \
                     guard in command.rs — the registry advertises a head eval never intercepts."
                );
            }
        }
    }
}
