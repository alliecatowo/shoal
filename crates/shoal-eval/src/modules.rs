//! Module loading for `use ./mod` (site/content/internals/language-conformance-contract.md, site/content/internals/roadmap-and-priorities.md). `use ./lib/deploy`
//! reads `./lib/deploy.shl` (resolved against the cwd; the `.shl` extension is
//! optional), evaluates it in a **fresh** scope, and binds its `export`ed decls
//! under the file stem (`deploy.build`, `deploy.version`) as a single record
//! value in the caller's environment. A module fn is therefore callable as
//! `deploy.build(...)` (the record-closure method-call path in `expr.rs`), and a
//! value export is readable as `deploy.version`.
//!
//! Modules are memoized per session by canonical path (a module evaluates once);
//! a circular `use` errors, naming the cycle. Non-exported decls stay
//! module-private (they are simply not lifted into the exports record, but remain
//! visible to the module's own fns via their captured scope).

use super::*;

impl Evaluator {
    /// Evaluate `use <path>`: load, memoize, and bind the module's exports under
    /// its file stem in the current environment.
    pub(crate) fn eval_use(&mut self, path: &str, span: Span) -> VResult<()> {
        let canon = self
            .resolve_module_path(path)
            .map_err(|e| e.or_span(span))?;
        let stem = canon
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string)
            .ok_or_else(|| {
                ErrorVal::new("arg_error", format!("module path has no name: {path}"))
                    .with_span(span)
            })?;
        let exports = self.load_module(&canon).map_err(|e| e.or_span(span))?;
        self.exec.shell.env.declare(stem, exports, false);
        Ok(())
    }

    /// Resolve a `use` path string to a canonical `.shl` file path. Tries the path
    /// as given, then with a `.shl` extension appended.
    fn resolve_module_path(&self, path: &str) -> VResult<PathBuf> {
        let base = PathBuf::from(path);
        let base = if base.is_absolute() {
            base
        } else {
            self.exec.shell.cwd.join(&base)
        };
        let candidates = if base.extension().is_some() {
            vec![base.clone()]
        } else {
            vec![base.with_extension("shl"), base.clone()]
        };
        for c in &candidates {
            if c.is_file() {
                return c.canonicalize().map_err(|e| {
                    ErrorVal::new("io_error", format!("cannot resolve module `{path}`: {e}"))
                });
            }
        }
        Err(ErrorVal::new(
            "not_found",
            format!(
                "module not found: `{path}` (looked for `{}`)",
                base.display()
            ),
        ))
    }

    /// Load (or return the memoized) exports record for a canonical module path.
    fn load_module(&mut self, canon: &Path) -> VResult<Value> {
        if let Some(cached) = self.exec.modules.cache.get(canon) {
            return Ok(cached.clone());
        }
        if self.exec.modules.stack.iter().any(|p| p == canon) {
            let mut cycle: Vec<String> = self
                .exec
                .modules
                .stack
                .iter()
                .map(|p| p.display().to_string())
                .collect();
            cycle.push(canon.display().to_string());
            return Err(ErrorVal::new(
                "custom",
                format!("circular `use`: {}", cycle.join(" -> ")),
            ));
        }
        let src = self
            .host
            .fs
            .read_to_string(canon)
            .map_err(|e| ErrorVal::new("io_error", format!("cannot read module: {e}")))?;
        let program =
            shoal_syntax::parse(&src).map_err(|e| ErrorVal::new("parse_error", e.to_string()))?;

        // Evaluate the module in a fresh scope: a new root env (so it cannot see
        // the caller's locals) rooted at the module file's own directory (so its
        // relative `use`/paths resolve against the module, not the caller).
        let saved_env = std::mem::replace(&mut self.exec.shell.env, Env::root());
        let module_dir = canon
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.exec.shell.cwd.clone());
        let saved_cwd = std::mem::replace(&mut self.exec.shell.cwd, module_dir);
        // A module's top-level decls are not inside a fn body; reset the guard so
        // module setup can run, then restore.
        let saved_in_fn = std::mem::replace(&mut self.exec.control.in_fn_body, 0);
        self.exec.modules.stack.push(canon.to_path_buf());

        let mut result = Ok(());
        for stmt in &program.stmts {
            if let Err(e) = self.eval_stmt(stmt, false) {
                result = Err(e);
                break;
            }
        }
        let exports = result.map(|()| self.collect_exports(&program));

        self.exec.modules.stack.pop();
        self.exec.shell.env = saved_env;
        self.exec.shell.cwd = saved_cwd;
        self.exec.control.in_fn_body = saved_in_fn;

        let exports = exports?;
        self.exec
            .modules
            .cache
            .insert(canon.to_path_buf(), exports.clone());
        Ok(exports)
    }

    /// After evaluating a module, lift its `export`ed top-level decls out of the
    /// module env into a record. Reads from `self.exec.shell.env`, which is still the module
    /// scope when this is called.
    fn collect_exports(&self, program: &Program) -> Value {
        let mut exports = Record::new();
        for stmt in &program.stmts {
            match stmt {
                Stmt::Fn { decl } if decl.exported => {
                    if let Some(v) = self.exec.shell.env.get(&decl.name) {
                        exports.insert(decl.name.clone(), v);
                    }
                }
                Stmt::Let {
                    pattern, exported, ..
                } if *exported => {
                    for name in pattern_names(pattern) {
                        if let Some(v) = self.exec.shell.env.get(&name) {
                            exports.insert(name, v);
                        }
                    }
                }
                _ => {}
            }
        }
        Value::Record(exports)
    }
}

/// The binder names introduced by a pattern (for lifting `export let` bindings).
fn pattern_names(pattern: &Pattern) -> Vec<String> {
    let mut names = Vec::new();
    collect_pattern_names(pattern, &mut names);
    names
}

fn collect_pattern_names(pattern: &Pattern, out: &mut Vec<String>) {
    match pattern {
        Pattern::Bind { name, .. } => out.push(name.clone()),
        Pattern::List { items, rest, .. } => {
            for p in items {
                collect_pattern_names(p, out);
            }
            if let Some(r) = rest {
                out.push(r.clone());
            }
        }
        Pattern::Record { fields, .. } => {
            for f in fields {
                match &f.pattern {
                    Some(p) => collect_pattern_names(p, out),
                    None => out.push(f.name.clone()),
                }
            }
        }
        _ => {}
    }
}
