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
        self.exec.shell.env.declare(stem, exports, false)?;
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
            if self.host.fs.is_file(c) {
                return self.host.fs.canonicalize(c).map_err(|e| {
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
        // Reserve capacity conceptually for every module currently evaluating:
        // nested `use`s must not over-admit merely because their parents have not
        // reached the memo table yet. This check precedes both file reads and AST
        // execution, so rejecting a new unique module cannot replay side effects.
        if self
            .exec
            .modules
            .cache
            .len()
            .saturating_add(self.exec.modules.stack.len())
            >= crate::exec_state::MAX_CACHED_MODULES
        {
            return Err(ErrorVal::new(
                "module_cache_limit",
                format!(
                    "session module memo limit reached ({})",
                    crate::exec_state::MAX_CACHED_MODULES
                ),
            )
            .with_hint("reuse a cached module or start a new evaluator session"));
        }
        let src = self.read_shoal_source(canon, "module")?;
        let program = shoal_syntax::parse_with_ctx(&src, self.isolated_parse_context())
            .map_err(|e| ErrorVal::new("parse_error", e.to_string()))?;

        // Evaluate the module in a fresh scope: a new root env (so it cannot see
        // the caller's locals) rooted at the module file's own directory (so its
        // relative `use`/paths resolve against the module, not the caller).
        let module_env = self.exec.shell.env.isolated();
        let saved_env = std::mem::replace(&mut self.exec.shell.env, module_env);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec_state::MAX_CACHED_MODULES;

    fn fill_cache(evaluator: &mut Evaluator, count: usize) {
        for index in 0..count {
            evaluator.exec.modules.cache.insert(
                PathBuf::from(format!("/memoized/module-{index}.shl")),
                Value::Record(Record::new()),
            );
        }
    }

    #[test]
    fn new_module_at_cap_is_rejected_before_top_level_side_effects() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("new.shl");
        std::fs::write(
            &module,
            r#""ran".save("module-side-effect")
export let answer = 42"#,
        )
        .unwrap();
        let mut evaluator = Evaluator::new(dir.path().to_path_buf());
        fill_cache(&mut evaluator, MAX_CACHED_MODULES);

        let error = evaluator.eval_use("./new", Span::default()).unwrap_err();
        assert_eq!(error.code, "module_cache_limit");
        assert!(!dir.path().join("module-side-effect").exists());
        assert_eq!(evaluator.exec.modules.cache.len(), MAX_CACHED_MODULES);
    }

    #[test]
    fn cached_module_remains_usable_at_cap_without_reexecution() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("cached.shl");
        std::fs::write(
            &module,
            r#""replayed".save("module-side-effect")
export let answer = 1"#,
        )
        .unwrap();
        let canon = module.canonicalize().unwrap();
        let mut evaluator = Evaluator::new(dir.path().to_path_buf());
        fill_cache(&mut evaluator, MAX_CACHED_MODULES - 1);
        let mut exports = Record::new();
        exports.insert("answer".into(), Value::Int(42));
        evaluator
            .exec
            .modules
            .cache
            .insert(canon, Value::Record(exports));

        evaluator
            .eval_use("./cached", Span::default())
            .expect("cached module should remain admissible at the cap");
        let Value::Record(bound) = evaluator.exec.shell.env.get("cached").unwrap() else {
            panic!("cached module binding should be a record");
        };
        assert_eq!(bound.get("answer"), Some(&Value::Int(42)));
        assert!(!dir.path().join("module-side-effect").exists());
        assert_eq!(evaluator.exec.modules.cache.len(), MAX_CACHED_MODULES);
    }

    #[test]
    fn oversized_sparse_module_fails_before_cache_admission_and_can_retry() {
        let dir = tempfile::tempdir().unwrap();
        let module = dir.path().join("large.shl");
        let file = std::fs::File::create(&module).unwrap();
        file.set_len((shoal_syntax::MAX_SOURCE_BYTES + 1) as u64)
            .unwrap();
        let mut evaluator = Evaluator::new(dir.path().to_path_buf());

        let error = evaluator.eval_use("./large", Span::default()).unwrap_err();
        assert_eq!(error.code, "source_too_large");
        assert!(error.msg.contains(&module.display().to_string()));
        assert!(evaluator.exec.modules.cache.is_empty());
        assert!(evaluator.exec.modules.stack.is_empty());

        std::fs::write(&module, "export let answer = 42\n").unwrap();
        evaluator
            .eval_use("./large", Span::default())
            .expect("a corrected module must remain loadable in the same session");
        assert_eq!(evaluator.exec.modules.cache.len(), 1);
        let Value::Record(exports) = evaluator.exec.shell.env.get("large").unwrap() else {
            panic!("module binding must be a record");
        };
        assert_eq!(exports.get("answer"), Some(&Value::Int(42)));
    }

    #[test]
    fn owned_production_paths_have_no_ambient_existence_or_canonicalization_probes() {
        let modules = include_str!("modules.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(!modules.contains("c.is_file()"));
        assert!(!modules.contains("c.canonicalize()"));

        let streams = include_str!("streams.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(!streams.contains("root.exists()"));
        assert!(!streams.contains("path.exists()"));

        let script = include_str!("script.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(!script.contains("resolved.exists()"));
    }
}
