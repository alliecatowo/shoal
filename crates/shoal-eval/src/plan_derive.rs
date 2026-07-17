//! The AST walk that derives a conservative, concrete [`Plan`] (see
//! [`crate::plan`] for the split rationale). Never spawns or mutates;
//! per-builtin/adapter effect computation lives in [`crate::plan_effects`].

use super::*;

use crate::plan_effects::{parse_declared_effect, push_effect};

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
            Stmt::Let { init, .. } => self.plan_expr(init, functions, aliases, out, depth),
            Stmt::Assign { target, value, .. } => {
                self.plan_expr(value, functions, aliases, out, depth)?;
                // Persistent `env.NAME = …` is an environment write, not only
                // its RHS effects (A2). Any other target is traversed for
                // nested effects (e.g. `xs[f()] = v`).
                if let Expr::Field { recv, name, .. } = target
                    && matches!(&**recv, Expr::Var { name: ns, .. } if ns == "env")
                {
                    push_effect(
                        out,
                        Effect::EnvWrite {
                            names: vec![name.clone()],
                        },
                    );
                    Ok(())
                } else {
                    self.plan_expr(target, functions, aliases, out, depth)
                }
            }
            Stmt::Use { path, .. } => {
                // `use ./mod` reads the module file and executes every top-level
                // statement (A1). Record the read concretely; the module body is
                // arbitrary code, so cover it conservatively with `Opaque`.
                push_effect(
                    out,
                    Effect::FsRead {
                        paths: vec![self.plan_module_path(path)],
                    },
                );
                push_effect(out, Effect::Opaque);
                Ok(())
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
            Expr::Unary { expr, .. } => self.plan_expr(expr, functions, aliases, out, depth),
            Expr::Field { recv, name, .. } => {
                // A bare `path("f").read`/`.lines`/… (no parens) is a filesystem
                // read of the receiver path (A4).
                if is_path_read_method(name)
                    && let Some(p) = self.path_literal(recv)
                {
                    push_effect(out, Effect::FsRead { paths: vec![p] });
                }
                self.plan_expr(recv, functions, aliases, out, depth)
            }
            Expr::Index { recv, index, .. } => {
                self.plan_expr(recv, functions, aliases, out, depth)?;
                self.plan_expr(index, functions, aliases, out, depth)
            }
            Expr::MethodCall {
                recv, name, args, ..
            } => {
                // `.feed(cmd)` bypasses builtin/adapter dispatch and spawns the
                // command via run_argv (A9): resolve the command operand as an
                // external spawn, exactly like the runtime — handled before the
                // generic traversal so it is not mis-resolved as a builtin.
                if name == "feed" && args.pos.len() == 1 && args.named.is_empty() {
                    return self.plan_feed(recv, &args.pos[0], functions, aliases, out, depth);
                }
                // `http.get/post/put/delete(url, …)` declares a `net.connect`
                // effect for leash + plan (site/content/internals/roadmap-and-priorities.md). The host is parsed from a
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
                // `.save`/`.append` write the path argument (A4). A dynamic path
                // cannot be bounded, so it plans as `Opaque` (approval).
                if matches!(name.as_str(), "save" | "append") {
                    let path = args.pos.first().and_then(|a| self.path_literal(a));
                    self.plan_save(path, out);
                }
                // Filesystem-backed path reads (`.read`/`.lines`/…) of a path
                // literal receiver (A4).
                if is_path_read_method(name)
                    && let Some(p) = self.path_literal(recv)
                {
                    push_effect(out, Effect::FsRead { paths: vec![p] });
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
        // Redirects are effects independent of the head (A3): `>`/`>>` write the
        // target, `< file` reads it. Recorded first so an aliased/functioned head
        // (whose body redirects belong to a different call) still gets this call's
        // redirects.
        self.plan_redirects(call, out);
        if let Some(target) = aliases.get(&call.head) {
            return self.plan_call(target, functions, aliases, out, depth + 1);
        }
        if let Some(body) = functions.get(&call.head) {
            return self.plan_block(body, functions, aliases, out, depth + 1);
        }
        if builtins::is_builtin(&call.head)
            || matches!(call.head.as_str(), "cd" | "pwd" | "j" | "jump")
        {
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
            // Derive a real binary-content hash for the plan (site/content/internals/language-conformance-contract.md): resolve
            // the adapter's bin and hash it, matching reef/leash's blake3-hex so
            // the hash a plan renders is the one a `proc_spawn` pin would check.
            // Falls back to an empty hash when the tool isn't installed/locatable
            // (name-only matching, as before) — planning never spawns or mutates.
            let bin_hash = self
                .hash_resolved_bin(OsStr::new(&adapter.bin))
                .unwrap_or_default();
            push_effect(
                out,
                Effect::ProcSpawn {
                    bin_hash,
                    argv0: adapter.bin,
                },
            );
        } else {
            push_effect(out, Effect::Opaque);
        }
        Ok(())
    }

    /// Command redirects are filesystem effects independent of the head (A3):
    /// `>`/`>>` write the target path, `< file` reads it. A dynamic (non-literal)
    /// write target cannot be bounded, so it plans as `Opaque` (approval).
    fn plan_redirects(&self, call: &CmdCall, out: &mut Vec<Effect>) {
        for r in &call.redirects {
            match r.kind {
                // Truncate (`>`) and append (`>>`) both write the target; the
                // effect vocabulary records the write, and append vs truncate is
                // distinguished only in that a truncate clobbers prior bytes.
                RedirectKind::Out | RedirectKind::Append => {
                    match self.cmd_arg_path_literal(&r.target) {
                        Some(p) => push_effect(out, Effect::FsWrite { paths: vec![p] }),
                        None => push_effect(out, Effect::Opaque),
                    }
                }
                RedirectKind::In => {
                    if let Some(p) = self.cmd_arg_path_literal(&r.target) {
                        push_effect(out, Effect::FsRead { paths: vec![p] });
                    }
                }
            }
        }
    }

    /// Absolutize a literal path string against the session cwd.
    fn plan_abs(&self, s: &str) -> PathBuf {
        let p = PathBuf::from(s);
        if p.is_absolute() { p } else { self.cwd.join(p) }
    }

    /// The literal path a command argument names, absolutized against cwd, or
    /// `None` when the argument is not a static literal.
    fn cmd_arg_path_literal(&self, arg: &CmdArg) -> Option<PathBuf> {
        let s = match arg {
            CmdArg::Word { text, .. } | CmdArg::Path { text, .. } => text.clone(),
            CmdArg::Str { expr, .. } | CmdArg::Expr { expr, .. } => match expr {
                Expr::Str { value, .. } => value.clone(),
                _ => return None,
            },
            _ => return None,
        };
        Some(self.plan_abs(&s))
    }

    /// Resolve a `use <path>` module string to the file the loader will read
    /// (A1): cwd-relative, preferring a `.shl` sibling, canonicalized best-effort.
    /// Planning never touches the file's contents.
    fn plan_module_path(&self, path: &str) -> PathBuf {
        let base = PathBuf::from(path);
        let base = if base.is_absolute() {
            base
        } else {
            self.cwd.join(&base)
        };
        let candidate = if base.extension().is_some() {
            base
        } else {
            let with_shl = base.with_extension("shl");
            if with_shl.is_file() { with_shl } else { base }
        };
        candidate.canonicalize().unwrap_or(candidate)
    }

    /// The literal path an expression names — a string literal or a
    /// `path("literal")` constructor — absolutized against cwd. `None` for a
    /// dynamic expression the planner cannot statically resolve.
    fn path_literal(&self, e: &Expr) -> Option<PathBuf> {
        str_literal(e).map(|s| self.plan_abs(&s))
    }

    /// Push a concrete external `ProcSpawn` for `head`, resolving and hashing the
    /// binary the same way the runtime spawn gate does (empty hash when the tool
    /// is not locatable — name-only matching). Planning never spawns.
    fn plan_external_spawn(&self, head: &str, out: &mut Vec<Effect>) {
        let bin_hash = self.hash_resolved_bin(OsStr::new(head)).unwrap_or_default();
        push_effect(
            out,
            Effect::ProcSpawn {
                bin_hash,
                argv0: head.to_string(),
            },
        );
    }

    /// A `.save`/`.append` sink: the resolved path writes, or an unbounded
    /// (dynamic) destination requires approval.
    fn plan_save(&self, path: Option<PathBuf>, out: &mut Vec<Effect>) {
        match path {
            Some(p) => push_effect(out, Effect::FsWrite { paths: vec![p] }),
            None => push_effect(out, Effect::Opaque),
        }
    }

    /// Mirror `eval_feed`'s operand classification: a command-shaped node (an
    /// interpreter block, a command call, or a bare non-variable name) is the
    /// command operand; the other operand is the value.
    fn is_command_operand(&self, e: &Expr) -> bool {
        match e {
            Expr::LangBlock { .. } | Expr::Cmd { .. } => true,
            Expr::Var { name, .. } => self.env.get(name).is_none(),
            _ => false,
        }
    }

    /// Plan `value.feed(cmd)` / `cmd.feed(value)` (A4, A9): plan the value
    /// operand's effects and derive an **external** spawn for the command
    /// operand, matching the runtime `.feed` path (which calls `run_argv`
    /// directly and never consults builtin/adapter dispatch).
    fn plan_feed(
        &mut self,
        recv: &Expr,
        arg: &Expr,
        functions: &std::collections::HashMap<String, Block>,
        aliases: &std::collections::HashMap<String, CmdCall>,
        out: &mut Vec<Effect>,
        depth: usize,
    ) -> VResult<()> {
        let (value_expr, cmd_expr) = if self.is_command_operand(recv) {
            (arg, recv)
        } else {
            (recv, arg)
        };
        self.plan_expr(value_expr, functions, aliases, out, depth)?;
        match cmd_expr {
            // An interpreter block runs an arbitrary program through its tool.
            Expr::LangBlock { .. } => push_effect(out, Effect::Opaque),
            Expr::Cmd { call, .. } => self.plan_external_spawn(&call.head, out),
            Expr::Var { name, .. } => self.plan_external_spawn(name, out),
            other => self.plan_expr(other, functions, aliases, out, depth)?,
        }
        Ok(())
    }
}

/// The path-reading `path` methods, all routed through the Fs port at runtime
/// (`path_fs_method`) and therefore filesystem reads for planning (A4).
fn is_path_read_method(name: &str) -> bool {
    matches!(
        name,
        "read" | "read_bytes" | "lines" | "exists" | "is_dir" | "is_file" | "size" | "modified"
    )
}

/// The literal path/string an expression names, if statically knowable: a
/// string literal, or a `path("literal")` constructor wrapping one. Used to
/// resolve `.save`/`.read`/`run`/`open` targets without executing anything.
fn str_literal(e: &Expr) -> Option<String> {
    match e {
        Expr::Str { value, .. } => Some(value.clone()),
        Expr::FnCall { name, args, .. }
            if name == "path" && args.named.is_empty() && args.pos.len() == 1 =>
        {
            str_literal(&args.pos[0])
        }
        _ => None,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A7: an adapter that declares a now-recognized `proc.spawn(...)` plus an
    /// unrecognized effect kind. The spawn must be derived (it was silently
    /// dropped before), and the unknown kind must plan as opaque — never
    /// silently ignored (fail-closed).
    #[test]
    fn adapter_effect_vocabulary_is_exhaustive_and_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("fixture.toml"),
            r#"[cmd.deployer]
bin="/bin/true"
effects=["proc.spawn(container)", "net.connect(registry:443)", "quantum.entangle(qubit)"]
"#,
        )
        .unwrap();
        let (catalog, warnings) = AdapterCatalog::load_dir(dir.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut ev = Evaluator::new(dir.path().into());
        ev.set_adapters(catalog);
        let effects = ev
            .plan_program(&shoal_syntax::parse("deployer").unwrap())
            .unwrap()
            .effects;
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::ProcSpawn { argv0, .. } if argv0 == "container")),
            "declared proc.spawn was not derived: {effects:?}"
        );
        assert!(
            effects.iter().any(
                |e| matches!(e, Effect::NetConnect { host, port } if host == "registry" && *port == 443)
            ),
            "declared net.connect was not derived: {effects:?}"
        );
        assert!(
            effects.contains(&Effect::Opaque),
            "unknown adapter effect kind was silently dropped: {effects:?}"
        );
    }

    /// A7 (unit): every effect kind in the vocabulary parses to its concrete
    /// effect; a bare unknown token plans as opaque, never dropped.
    #[test]
    fn declared_effect_covers_full_vocabulary() {
        use crate::plan_effects::parse_declared_effect;
        let cwd = Path::new("/tmp");
        let b = std::collections::HashMap::new();
        assert_eq!(
            parse_declared_effect("net.listen(8080)", &b, cwd),
            vec![Effect::NetListen { port: 8080 }]
        );
        assert_eq!(
            parse_declared_effect("env.write(TOKEN)", &b, cwd),
            vec![Effect::EnvWrite {
                names: vec!["TOKEN".into()]
            }]
        );
        assert_eq!(
            parse_declared_effect("secret.use(github)", &b, cwd),
            vec![Effect::SecretUse {
                names: vec!["github".into()]
            }]
        );
        assert_eq!(
            parse_declared_effect("session.write", &b, cwd),
            vec![Effect::SessionWrite]
        );
        assert_eq!(parse_declared_effect("time", &b, cwd), vec![Effect::Time]);
        assert_eq!(
            parse_declared_effect("bogus.kind(x)", &b, cwd),
            vec![Effect::Opaque]
        );
        assert_eq!(
            parse_declared_effect("bare-nonsense", &b, cwd),
            vec![Effect::Opaque]
        );
    }

    /// Derive the effect list for `src` under a fresh evaluator rooted at `cwd`.
    fn effects_at(cwd: &Path, src: &str) -> Vec<Effect> {
        let mut ev = Evaluator::new(cwd.to_path_buf());
        ev.plan_program(&shoal_syntax::parse(src).unwrap())
            .unwrap()
            .effects
    }

    /// A1: `use ./mod` reads the module file and covers its body conservatively.
    #[test]
    fn use_reads_module_and_covers_body() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mod.shl"), "export let x = 1").unwrap();
        let effects = effects_at(dir.path(), "use ./mod");
        assert!(
            effects.iter().any(
                |e| matches!(e, Effect::FsRead { paths } if paths.iter().any(|p| p.ends_with("mod.shl")))
            ),
            "use did not read the module: {effects:?}"
        );
        assert!(
            effects.contains(&Effect::Opaque),
            "module body not covered: {effects:?}"
        );
    }

    /// A2: persistent `env.NAME = …` is an EnvWrite naming NAME.
    #[test]
    fn env_assignment_is_env_write() {
        let dir = tempfile::tempdir().unwrap();
        let effects = effects_at(dir.path(), "env.AUDIT_ONLY = \"y\"");
        assert!(
            effects.contains(&Effect::EnvWrite {
                names: vec!["AUDIT_ONLY".into()]
            }),
            "env.NAME = … did not plan an EnvWrite: {effects:?}"
        );
    }

    /// A3: `>`/`>>` write the redirect target; `< file` reads it.
    #[test]
    fn redirects_derive_fs_effects() {
        let dir = tempfile::tempdir().unwrap();
        let trunc = effects_at(dir.path(), "echo hi > out.txt");
        assert!(
            trunc.contains(&Effect::FsWrite {
                paths: vec![dir.path().join("out.txt")]
            }),
            "`> out.txt` did not plan a write: {trunc:?}"
        );
        let append = effects_at(dir.path(), "echo hi >> out.txt");
        assert!(
            append.contains(&Effect::FsWrite {
                paths: vec![dir.path().join("out.txt")]
            }),
            "`>> out.txt` did not plan a write: {append:?}"
        );
        let read = effects_at(dir.path(), "cat < in.txt");
        assert!(
            read.contains(&Effect::FsRead {
                paths: vec![dir.path().join("in.txt")]
            }),
            "`< in.txt` did not plan a read: {read:?}"
        );
    }

    /// A4: `.save`/`.append` write the path argument; path reads read it.
    #[test]
    fn method_save_append_and_path_read() {
        let dir = tempfile::tempdir().unwrap();
        let save = effects_at(dir.path(), "\"x\".save(\"p\")");
        assert!(
            save.contains(&Effect::FsWrite {
                paths: vec![dir.path().join("p")]
            }),
            ".save did not plan a write: {save:?}"
        );
        let append = effects_at(dir.path(), "\"x\".append(\"p\")");
        assert!(
            append.contains(&Effect::FsWrite {
                paths: vec![dir.path().join("p")]
            }),
            ".append did not plan a write: {append:?}"
        );
        // Bare `.read` (Field form) and `.read()` (MethodCall form) both read.
        for src in ["path(\"f\").read", "path(\"f\").read()"] {
            let read = effects_at(dir.path(), src);
            assert!(
                read.contains(&Effect::FsRead {
                    paths: vec![dir.path().join("f")]
                }),
                "`{src}` did not plan a read: {read:?}"
            );
        }
    }

    /// A9: `.feed(cat)` spawns cat externally (matching run_argv), not the
    /// `cat` builtin — no FsRead, a concrete ProcSpawn.
    #[test]
    fn feed_resolves_command_as_external_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let effects = effects_at(dir.path(), "\"x\".feed(cat)");
        assert!(
            effects
                .iter()
                .any(|e| matches!(e, Effect::ProcSpawn { argv0, .. } if argv0 == "cat")),
            ".feed(cat) did not spawn cat: {effects:?}"
        );
        assert!(
            !effects.iter().any(|e| matches!(e, Effect::FsRead { .. })),
            ".feed(cat) mis-resolved cat as the builtin (FsRead): {effects:?}"
        );
    }
}
