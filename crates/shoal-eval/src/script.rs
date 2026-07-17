//! `with` dynamic scoping, `spawn` blocks, and the `run`/script-file family
//! (poly runner, `.shl` interpreter, `.sh`/`.py`/`.js` interpreters, `.rs`
//! rust-script/rustc fallback).

use super::*;

impl Evaluator {
    pub(crate) fn eval_with(
        &mut self,
        cwd: Option<&Expr>,
        env_expr: Option<&Expr>,
        reef_expr: Option<&Expr>,
        body: &Block,
    ) -> VResult<Value> {
        let old_cwd = self.exec.shell.cwd.clone();
        let old_env = self.exec.shell.process_env.clone();
        if let Some(e) = cwd {
            match self.eval_expr(e, Position::Value)? {
                Value::Path(p) => {
                    self.exec.shell.cwd = if p.is_absolute() {
                        p
                    } else {
                        self.exec.shell.cwd.join(p)
                    }
                }
                Value::Str(s) => self.exec.shell.cwd = self.exec.shell.cwd.join(s),
                _ => return Err(ErrorVal::new("type_error", "with cwd expects path")),
            }
        }
        if let Some(e) = env_expr {
            let Value::Record(r) = self.eval_expr(e, Position::Value)? else {
                return Err(ErrorVal::new("type_error", "with env expects record"));
            };
            for (k, v) in r {
                let val = self.argv_value(v)?;
                self.exec
                    .shell
                    .process_env
                    .retain(|(n, _)| n != &OsString::from(&k));
                self.exec.shell.process_env.push((k.into(), val));
            }
        }
        // `with reef: {tool: constraint, …} { }` — dynamic reef scoping
        // (site/content/internals/reef-resolution.md), pushed as an override layer for the block's dynamic
        // extent and popped on every exit path below, mirroring cwd/env.
        let mut pushed_reef = false;
        if let Some(e) = reef_expr {
            let Value::Record(r) = self.eval_expr(e, Position::Value)? else {
                self.exec.shell.cwd = old_cwd;
                self.exec.shell.process_env = old_env;
                return Err(ErrorVal::new("type_error", "with reef expects record"));
            };
            if let Err(err) = self.push_reef_override(&r) {
                self.exec.shell.cwd = old_cwd;
                self.exec.shell.process_env = old_env;
                return Err(err);
            }
            pushed_reef = true;
        }
        let out = self.block_value(body);
        if pushed_reef {
            self.pop_reef_override();
        }
        self.exec.shell.cwd = old_cwd;
        self.exec.shell.process_env = old_env;
        out
    }
    pub(crate) fn spawn_block(&mut self, body: Block) -> VResult<Value> {
        let lease = self.host.native_workers.acquire()?;
        let task = shoal_value::TaskVal::new("spawn block");
        // Structured cancellation: cancelling the task cancels the child's exec
        // tokens (defect #14) — a FRESH token wired to the task's cancel hook.
        let child_cancel = CancelToken::new();
        let hook_cancel = child_cancel.clone();
        task.on_cancel(Box::new(move || hook_cancel.cancel()));
        let worker = task.clone();
        // The one authoritative child constructor (HR-B1): it inherits the full
        // session context — leash policy/principal, reef scope/resolver/
        // overrides, config, all effect ports, the event bus, and session
        // identity — by construction, not the partial hand-copy the audit
        // (B1–B4) found here dropping leash/reef/config. `Inherit` scope: a
        // `spawn` body sees the caller's bindings.
        let ctx = self.child_context();
        // Register before launch so a fast worker cannot finish before the task
        // becomes discoverable. If launch itself fails, finish that registered
        // task with the same stable failure returned to the caller.
        self.exec.jobs.register(task.clone());
        let launch = std::thread::Builder::new()
            .name("shoal-spawn-block".into())
            .spawn(move || {
                let _lease = lease;
                let mut ev = ctx.build(ChildKind::Spawn, child_cancel);
                worker.finish(ev.block_value(&body));
            });
        if let Err(error) = launch {
            let failure = ErrorVal::new(
                "task_spawn",
                format!("could not start spawn worker: {error}"),
            );
            task.finish(Err(failure.clone()));
            return Err(failure);
        }
        Ok(Value::Task(task))
    }

    /// `run(<path>, …)` / `run(<name>, …)` — the poly runner + dynamic form.
    pub(crate) fn run_poly(
        &mut self,
        target: Value,
        args: Vec<Value>,
        position: Position,
    ) -> VResult<Value> {
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
        let is_path = name.contains('/') || name.starts_with('.') || name.starts_with('~');
        let resolved = {
            let p = self.resolve_path(&name);
            if p.is_absolute() {
                p
            } else {
                self.exec.shell.cwd.join(p)
            }
        };
        let ext = Path::new(&name)
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        // A bare filename (no path separator) is still scripty when its
        // extension is one the runner machinery actually knows (site/content/internals/values-streams-execution.md
        // "plain filename in cwd" ergonomics case) — sourced from the SAME
        // `RunnerTable` `run_script_file`/reef itself consult (shipped
        // defaults `py js ts sh shl rb lua`, plus any in-scope manifest's
        // `[runners]` overlay), never a separately hand-maintained list that
        // can drift from runner.rs again (site/content/internals/reef-resolution.md). `rs` is special-cased:
        // it intentionally has no default runner-table entry (compile-vs-
        // script ambiguity, site/content/internals/reef-resolution.md) but IS handled by `run_script_file`'s
        // own rustc/rust-script fallback, so it stays scripty for symmetry
        // with the `./x.rs` path form.
        let scripty = ext.as_deref().is_some_and(|e| {
            e == "rs" || self.reef_chain_snapshot().runner_table().get(e).is_some()
        });
        if is_path && ext.is_none() && self.looks_like_native_executable(&resolved) {
            let mut argv = vec![resolved.into_os_string()];
            for value in args {
                argv.push(self.argv_value(value)?);
            }
            return self.run_argv(argv, position, StdinSpec::Null, &[], Span::default(), None);
        }
        if is_path || (scripty && self.host.fs.exists(&resolved)) {
            debug_assert_eq!(
                self.resolve_dynamic_run(&name, true).source,
                shoal_syntax::commands::CommandSource::Runner
            );
            return self.run_script_file(&resolved, ext.as_deref(), args, position);
        }
        // Dynamic command invocation (value semantics like any command).
        debug_assert_eq!(
            self.resolve_dynamic_run(&name, false).source,
            shoal_syntax::commands::CommandSource::External
        );
        let mut argv = vec![OsString::from(&name)];
        for v in args {
            argv.push(self.argv_value(v)?);
        }
        self.run_argv(argv, position, StdinSpec::Null, &[], Span::default(), None)
    }

    /// Distinguish an extensionless native executable from extensionless
    /// Shoal source without escaping the session's filesystem capability.
    fn looks_like_native_executable(&self, path: &Path) -> bool {
        use std::io::Read;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if !self.host.fs.metadata(path).is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            }) {
                return false;
            }
        }
        let mut magic = [0u8; 4];
        let Ok(mut file) = self.host.fs.open_read(path) else {
            return false;
        };
        if file.read_exact(&mut magic).is_err() {
            return false;
        }
        matches!(
            magic,
            [0x7f, b'E', b'L', b'F']
                | [0xfe, 0xed, 0xfa, 0xce]
                | [0xfe, 0xed, 0xfa, 0xcf]
                | [0xce, 0xfa, 0xed, 0xfe]
                | [0xcf, 0xfa, 0xed, 0xfe]
                | [0xca, 0xfe, 0xba, 0xbe]
                | [0xbe, 0xba, 0xfe, 0xca]
        )
    }

    pub(crate) fn run_script_file(
        &mut self,
        path: &Path,
        ext: Option<&str>,
        args: Vec<Value>,
        position: Position,
    ) -> VResult<Value> {
        match ext {
            Some("shl") | None => {
                let src = self.read_shoal_source(path, "script")?;
                let program = shoal_syntax::parse(&src)
                    .map_err(|e| ErrorVal::new("parse_error", e.to_string()))?;
                // A `.shl` script is a separate program (see
                // `site/content/internals/values-streams-execution.md`):
                // `ChildKind::Script` keeps a fresh root lexical scope,
                // so its `let`s do not leak back into the caller session
                // (`Env::clone` would share the same Arc'd scope and leak them).
                // Via the one child constructor (HR-B1) it still inherits the
                // audited session context — leash/reef/config/ports/bus/session
                // identity, which the old hand-copy here dropped for leash/reef
                // (audit B1–B3) — plus the parent's cancellation so a host
                // cancel interrupts the script.
                let cancel = self.cancellation_token();
                let mut child = self.child_context().build(ChildKind::Script, cancel);
                child.env_mut().declare("args", Value::List(args), false)?;
                child
                    .env_mut()
                    .declare("script", Value::Path(path.to_path_buf()), false)?;
                child.eval_program(&program)
            }
            _ => {
                // reef runner resolution (site/content/internals/reef-resolution.md): when a manifest is in scope,
                // the `[runners]` table (ext → tool, shebang fallback) picks the
                // interpreter, whose tool the spawn then reef-resolves. Falls
                // back to today's fixed interpreters when no manifest applies.
                if let Some(mut argv) = self.reef_runner_argv(path) {
                    argv.push(path.as_os_str().to_owned());
                    for v in args {
                        argv.push(self.argv_value(v)?);
                    }
                    return self.run_argv(
                        argv,
                        position,
                        StdinSpec::Null,
                        &[],
                        Span::default(),
                        None,
                    );
                }
                match ext {
                    Some("sh") => self.run_interp("sh", path, args, position),
                    Some("py") => self.run_interp("python3", path, args, position),
                    Some("js") => self.run_interp("node", path, args, position),
                    Some("rs") => self.run_rust_script(path, args, position),
                    _ => {
                        // Extension not in any `[runners]` table and not a
                        // built-in interpreter resolution exhausted:
                        // fall back to the file's shebang (step 2), else raise
                        // `runner_not_found` (step 3) instead of blindly
                        // exec'ing an unresolvable path.
                        if let Some(mut argv) = self.shebang_argv(path) {
                            argv.push(path.as_os_str().to_owned());
                            for v in args {
                                argv.push(self.argv_value(v)?);
                            }
                            return self.run_argv(
                                argv,
                                position,
                                StdinSpec::Null,
                                &[],
                                Span::default(),
                                None,
                            );
                        }
                        Err(ErrorVal::new(
                            "runner_not_found",
                            format!("no runner for {}", path.display()),
                        )
                        .with_hint(
                            "configured runners: py js ts sh shl rb lua — add one under \
                             `[runners]` in a `.reef.toml`, or give the file a `#!` shebang"
                                .to_string(),
                        ))
                    }
                }
            }
        }
    }

    /// Shebang-fallback runner resolution: read the file's
    /// first line; if it is `#!<interp> [args…]`, return the interpreter argv
    /// prefix. `#!/usr/bin/env <tool>` resolves to `<tool>` (env-style). `None`
    /// when the file is unreadable or has no shebang.
    pub(crate) fn shebang_argv(&self, path: &Path) -> Option<Vec<OsString>> {
        let first = self.read_shebang_line(path)?;
        let rest = first.strip_prefix("#!")?.trim();
        let mut words = rest.split_whitespace();
        let interp = words.next()?;
        let mut argv: Vec<OsString> = Vec::new();
        let base = Path::new(interp)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(interp);
        if base == "env" {
            // `#!/usr/bin/env python` → the real interpreter is the next word.
            argv.push(OsString::from(words.next()?));
        } else {
            argv.push(OsString::from(interp));
        }
        argv.extend(words.map(OsString::from));
        Some(argv)
    }

    pub(crate) fn run_interp(
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

    pub(crate) fn run_rust_script(
        &mut self,
        path: &Path,
        args: Vec<Value>,
        position: Position,
    ) -> VResult<Value> {
        let path_env = self
            .exec
            .shell
            .process_env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.as_os_str());
        if shoal_exec::which(OsStr::new("rust-script"), path_env).is_some() {
            return self.run_interp("rust-script", path, args, position);
        }
        // Fall back to compiling with rustc into a private per-invocation
        // directory. Keeping the TempDir guard alive through execution makes
        // same-stem concurrent scripts collision-free and removes both the
        // compiler output and directory on every Result exit path.
        let (artifact, bin) = rust_script_artifact_in(&std::env::temp_dir()).map_err(|error| {
            ErrorVal::new(
                "runner_not_found",
                format!("cannot create a temporary Rust-script artifact: {error}"),
            )
        })?;
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
        let result = self.run_argv(argv, position, StdinSpec::Null, &[], Span::default(), None);
        drop(artifact);
        result
    }
}

fn rust_script_artifact_in(parent: &Path) -> std::io::Result<(tempfile::TempDir, PathBuf)> {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::Builder::new()
        .prefix("shoal-rs-")
        .tempdir_in(parent)?;
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
    let bin = dir.path().join("script");
    Ok((dir, bin))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_shoal_script_is_typed_and_a_corrected_retry_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("large.shl");
        let file = std::fs::File::create(&script).unwrap();
        file.set_len((shoal_syntax::MAX_SOURCE_BYTES + 1) as u64)
            .unwrap();
        let mut evaluator = Evaluator::new(dir.path().to_path_buf());

        let error = evaluator
            .run_script_file(&script, Some("shl"), Vec::new(), Position::Value)
            .unwrap_err();
        assert_eq!(error.code, "source_too_large");
        assert!(error.msg.contains(&script.display().to_string()));

        std::fs::write(&script, "42\n").unwrap();
        assert_eq!(
            evaluator
                .run_script_file(&script, Some("shl"), Vec::new(), Position::Value)
                .unwrap(),
            Value::Int(42)
        );
    }

    #[test]
    fn extensionless_native_executable_path_runs_as_a_process() {
        let mut evaluator = Evaluator::new(std::env::current_dir().unwrap());
        let program = shoal_syntax::parse(r#"run("/bin/true")"#).unwrap();
        let Value::Outcome(outcome) = evaluator.eval_program(&program).unwrap() else {
            panic!("native executable should return an outcome")
        };
        assert!(outcome.ok);
    }

    #[test]
    fn shebang_reads_only_a_bounded_utf8_header() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("binary-body");
        std::fs::write(&script, b"#!/usr/bin/env python\n\xff\xfe").unwrap();
        let evaluator = Evaluator::new(dir.path().to_path_buf());
        assert_eq!(
            evaluator.shebang_argv(&script),
            Some(vec![OsString::from("python")])
        );

        std::fs::write(&script, format!("#!{}", "x".repeat(9 * 1024))).unwrap();
        assert_eq!(evaluator.shebang_argv(&script), None);
    }
    use crate::host_services::NativeWorkerBudget;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn spawn_quota_rejects_then_reclaims_after_task_cancellation() {
        static PROCESS: AtomicUsize = AtomicUsize::new(0);
        let mut evaluator = Evaluator::new(std::env::temp_dir());
        Arc::make_mut(&mut evaluator.host).native_workers =
            NativeWorkerBudget::with_limits(1, &PROCESS, 8);

        let program =
            shoal_syntax::parse("let held = spawn { sleep 10s }\nspawn { null }").unwrap();
        let error = evaluator.eval_program(&program).unwrap_err();
        assert_eq!(error.code, "session_worker_limit");
        let Value::Task(held) = evaluator.exec.shell.env.get("held").unwrap() else {
            panic!("the admitted spawn should remain bound as a task");
        };
        held.cancel();
        held.wait().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        while PROCESS.load(Ordering::Relaxed) != 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(PROCESS.load(Ordering::Relaxed), 0);

        let replacement = evaluator
            .eval_program(&shoal_syntax::parse("spawn { null }").unwrap())
            .expect("completed/cancelled spawn should return its worker slot");
        let Value::Task(replacement) = replacement else {
            panic!("replacement spawn should return a task");
        };
        replacement.wait().unwrap();
    }

    #[test]
    fn rust_script_artifacts_are_unique_private_and_removed_on_drop() {
        use std::os::unix::fs::PermissionsExt;

        let parent = tempfile::tempdir().unwrap();
        let (first, first_bin) = rust_script_artifact_in(parent.path()).unwrap();
        let (second, second_bin) = rust_script_artifact_in(parent.path()).unwrap();
        assert_ne!(first.path(), second.path());
        assert_eq!(first_bin.file_name(), Some(OsStr::new("script")));
        assert_eq!(second_bin.file_name(), Some(OsStr::new("script")));
        assert_eq!(
            first.path().metadata().unwrap().permissions().mode() & 0o077,
            0
        );

        let first_path = first.path().to_path_buf();
        let second_path = second.path().to_path_buf();
        std::fs::write(&first_bin, b"artifact").unwrap();
        std::fs::write(&second_bin, b"artifact").unwrap();
        drop(first);
        assert!(!first_path.exists());
        assert!(second_path.exists());
        drop(second);
        assert!(!second_path.exists());
    }

    /// Fix 3: `run_poly`'s scripty gate used to hardcode `{shl,sh,py,js,rs}`,
    /// so a BARE filename (no `./`) with any other shipped-default extension
    /// (`ts`/`rb`/`lua`) misrouted to a literal-command-name lookup instead of
    /// the runner machinery — even though `RunnerTable::defaults()` has known
    /// this extension all along. Remapping `rb` to `sh` in this fixture's own
    /// manifest keeps the assertion host-independent (no real ruby install
    /// needed): the point is proving the bare name reached the runner
    /// dispatch at all, not that any particular interpreter is present.
    #[test]
    fn bare_filename_scripty_gate_honors_full_runner_table() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".reef.toml"),
            "[tools]\nplaceholder = \"*\"\n\n[runners]\nrb = \"sh\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("x.rb"), b"echo bare-rb-ran\n").unwrap();

        let mut ev = Evaluator::new(dir.path().to_path_buf());
        let out = ev
            .eval_program(&shoal_syntax::parse(r#"run("x.rb").out"#).unwrap())
            .expect("bare `x.rb` should route through the runner table, not command lookup");
        assert_eq!(out, Value::Str("bare-rb-ran".into()));
    }

    /// Before the fix, the SAME bare filename with no manifest in scope (so
    /// `chain.runner_table()` is unreachable and only the `"rs"` special-case
    /// applies) must still resolve — a regression guard for the refactor,
    /// not new behavior. `rustc` is guaranteed present (this test binary was
    /// built with it), so a trivial program actually compiles and runs.
    #[test]
    fn rs_stays_scripty_as_a_bare_filename_with_no_manifest() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("x.rs"), b"fn main() {}\n").unwrap();
        let mut ev = Evaluator::new(dir.path().to_path_buf());
        let out = ev
            .eval_program(&shoal_syntax::parse(r#"run("x.rs")"#).unwrap())
            .expect("bare `x.rs` should compile and run via rustc, not misroute to command lookup");
        let Value::Outcome(o) = out else {
            panic!("expected an outcome, got {out:?}")
        };
        assert!(o.ok, "the compiled no-op binary should exit 0");
    }
}
