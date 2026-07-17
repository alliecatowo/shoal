//! Host builtins that reach into IO directly (site/content/internals/language-conformance-contract.md): `interact`, `open`, `save`,
//! `parallel`, `retry`, plus the builtin-redirect/outcome-wrapping helpers they
//! (and `command.rs`) share.

use super::*;

impl Evaluator {
    pub(crate) fn apply_builtin_redirects(
        &mut self,
        call: &CmdCall,
        value: Value,
    ) -> VResult<Value> {
        let mut captured = false;
        for r in &call.redirects {
            match r.kind {
                RedirectKind::Out => {
                    let p = self.arg_path(&r.target)?;
                    // Undo (site/content/internals/language-conformance-contract.md): snapshot the target's prior bytes first, so
                    // `echo x > f` is reversible exactly like `cp`/`save`.
                    let undo_pre = self.redirect_undo_pre(&p);
                    self.host
                        .fs
                        .write(&p, &value_bytes(&value))
                        .map_err(|e| ErrorVal::new("custom", e.to_string()))?;
                    self.overwrite_undo_post(undo_pre);
                    captured = true;
                }
                RedirectKind::Append => {
                    let p = self.arg_path(&r.target)?;
                    let undo_pre = self.redirect_undo_pre(&p);
                    self.host
                        .fs
                        .append(&p, &value_bytes(&value))
                        .map_err(|e| ErrorVal::new("custom", e.to_string()))?;
                    self.overwrite_undo_post(undo_pre);
                    captured = true;
                }
                RedirectKind::In => {}
            }
        }
        // `cmd > file` / `>> file` sends the output to the file — it must not
        // also be rendered to the statement sink (defect #8). Yield Null so the
        // redirected statement stays silent on stdout.
        if captured { Ok(Value::Null) } else { Ok(value) }
    }

    /// Force a real PTY for `interact <cmd…>` (site/content/internals/language-conformance-contract.md).
    pub(crate) fn builtin_interact(&mut self, call: &CmdCall) -> VResult<Value> {
        let vs = self.collect_cmd_values(call)?;
        if vs.is_empty() {
            return Err(ErrorVal::arg_error("interact expects a command"));
        }
        let mut argv = Vec::new();
        for v in vs {
            argv.push(self.argv_value(v)?);
        }
        let saved = self.session.interactive;
        self.session.interactive = true;
        let r = self.run_argv(
            argv,
            Position::Statement,
            StdinSpec::Inherit,
            &[],
            call.span,
            None,
        );
        self.session.interactive = saved;
        r
    }

    /// `open <path>` — detached `xdg-open` (site/content/internals/language-conformance-contract.md).
    pub(crate) fn builtin_open(&mut self, pos: Vec<Value>) -> VResult<Value> {
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
        let p = if p.is_absolute() {
            p
        } else {
            self.exec.shell.cwd.join(p)
        };
        self.host
            .opener
            .open(&p)
            .map_err(|e| ErrorVal::new("custom", e))?;
        Ok(Value::Null)
    }

    /// `save(path, value)` builtin form (site/content/internals/language-conformance-contract.md) — delegates to the value method.
    pub(crate) fn builtin_save(&mut self, pos: Vec<Value>) -> VResult<Value> {
        if pos.len() != 2 {
            return Err(ErrorVal::arg_error("save expects (path, value)"));
        }
        let path = pos[0].clone();
        let value = pos[1].clone();
        // site/content/internals/language-conformance-contract.md undo: if `save` overwrites an existing file under a journal,
        // snapshot its prior bytes first, then record a restore inverse after
        // the write. A no-op unless a journal is installed mid-statement.
        let undo_pre = self.save_undo_pre(&path);
        let result = shoal_value::methods::call_method(
            self,
            value,
            "save",
            CallArgs {
                pos: vec![path],
                named: vec![],
            },
            Span::default(),
        );
        self.overwrite_undo_post(undo_pre);
        result
    }

    /// `parallel(...closures)` — fail-fast by default; `settle: true` collects all
    /// outcomes (site/content/internals/language-conformance-contract.md).
    pub(crate) fn builtin_parallel(&mut self, args: &Args) -> VResult<Value> {
        let a = self.eval_args(args)?;
        let settle = a
            .named
            .iter()
            .find(|(n, _)| n == "settle")
            .map(|(_, v)| matches!(v, Value::Bool(true)))
            .unwrap_or(false);
        // Reserve the whole batch before starting any thread. A quota failure
        // drops these RAII leases and runs zero closures, so retrying cannot
        // duplicate a partially-executed fan-out.
        let mut leases = Vec::with_capacity(a.pos.len());
        for _ in 0..a.pos.len() {
            leases.push(self.host.native_workers.acquire()?);
        }

        // Threads wait behind an atomic launch gate until every fallible named
        // spawn succeeds. An OS spawn failure aborts already-created workers
        // before they call user code, preserving the same all-or-none boundary
        // as quota admission without a poisonable mutex/condvar barrier.
        const WAITING: u8 = 0;
        const RUN: u8 = 1;
        const ABORT: u8 = 2;
        let gate = Arc::new(std::sync::atomic::AtomicU8::new(WAITING));
        let mut handles = Vec::new();
        for (index, (f, lease)) in a.pos.into_iter().zip(leases).enumerate() {
            // The one authoritative child constructor (HR-B1): each `parallel`
            // closure runs in a child that inherits the audited session context —
            // leash policy/principal, reef state, config, all effect ports, the
            // event bus, and session identity. The old hand-copy here shared
            // only ports (dropping leash, reef, config, and the bus), so a
            // command a policy forbids foreground could run unconfined inside a
            // `parallel` closure (audit B1–B4). `Inherit` scope: the closure
            // sees the caller's bindings. Inheriting the parent's cancellation
            // token makes cancelling the parent cancel the whole batch.
            let ctx = self.child_context();
            let cancel = self.cancellation_token();
            let worker_gate = gate.clone();
            match std::thread::Builder::new()
                .name(format!("shoal-parallel-{index}"))
                .spawn(move || {
                    let _lease = lease;
                    loop {
                        match worker_gate.load(std::sync::atomic::Ordering::Acquire) {
                            RUN => break,
                            ABORT => {
                                return Err(ErrorVal::new(
                                    "task_spawn",
                                    "parallel batch aborted before launch",
                                ));
                            }
                            _ => std::thread::park_timeout(Duration::from_millis(1)),
                        }
                    }
                    let mut ev = ctx.build(ChildKind::Parallel, cancel);
                    ev.call_value(&f, CallArgs::default())
                }) {
                Ok(handle) => handles.push(handle),
                Err(error) => {
                    gate.store(ABORT, std::sync::atomic::Ordering::Release);
                    for handle in handles {
                        let _ = handle.join();
                    }
                    return Err(ErrorVal::new(
                        "task_spawn",
                        format!("could not start parallel worker: {error}"),
                    ));
                }
            }
        }
        gate.store(RUN, std::sync::atomic::Ordering::Release);
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

    /// `retry(n, thunk, delay: duration?)` — retry a thunk until it succeeds (site/content/internals/language-conformance-contract.md).
    pub(crate) fn builtin_retry(&mut self, args: &Args) -> VResult<Value> {
        let a = self.eval_args(args)?;
        let n = match a.pos.first() {
            Some(Value::Int(i)) if *i > 0 => *i as usize,
            _ => {
                return Err(ErrorVal::arg_error(
                    "retry expects a positive attempt count",
                ));
            }
        };
        let thunk = a
            .pos
            .get(1)
            .cloned()
            .ok_or_else(|| ErrorVal::arg_error("retry expects a thunk"))?;
        let delay = a
            .named
            .iter()
            .find(|(k, _)| k == "delay")
            .and_then(|(_, v)| {
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
}

/// Wrap a builtin's structured result in a `Value::Outcome` (outcome
/// unification, P1a). The structured value becomes the outcome's `parsed`
/// (`.out`); `stdout` carries the same bytes a redirect/`echo … > file` would
/// write, so `echo`, `ls`, `stat`, `which`, … all compose and forward like
/// external outcomes. Builtin outcomes are marked `pid == 0` and `streamed ==
/// false` (they never reach a PTY) so the statement sink and result renderer
/// still render their `.out` (defect #1).
pub(crate) fn builtin_outcome(head: &str, result: Value) -> Value {
    let stdout = value_bytes(&result);
    Value::Outcome(Arc::new(OutcomeVal {
        status: Some(0),
        signal: None,
        ok: true,
        stdout: Arc::new(stdout),
        // Builtin outcomes are always fully resident (no capture spill).
        stdout_ref: None,
        stderr: Arc::new(Vec::new()),
        dur_ns: 0,
        pid: 0,
        cmd: head.to_string(),
        parsed: Some(result),
        streamed: false,
        // No invocation span in scope here: `builtin_outcome` is handed only a
        // head string and an already-computed result value, not the call site.
        // Honestly `None` (the wire omits it) rather than fabricating one.
        span: None,
    }))
}

/// Render a value to bytes for a builtin redirect target (defect #8).
pub(crate) fn value_bytes(v: &Value) -> Vec<u8> {
    match v {
        Value::Bytes(b) => (**b).clone(),
        // site/content/internals/language-conformance-contract.md: a CAS-backed value writes its FULL content (loaded on demand),
        // falling back to the resident preview only if the store is unreachable.
        Value::CasBytes(c) => c.resolve().unwrap_or_else(|_| c.preview.as_ref().clone()),
        Value::Str(s) => {
            let mut b = s.clone().into_bytes();
            if !s.ends_with('\n') {
                b.push(b'\n');
            }
            b
        }
        Value::Outcome(o) => o.stdout_bytes().unwrap_or_else(|_| (*o.stdout).clone()),
        Value::Null => Vec::new(),
        other => {
            let mut b = crate::helpers::display_top(other).into_bytes();
            b.push(b'\n');
            b
        }
    }
}

#[cfg(test)]
mod worker_tests {
    use super::*;
    use crate::host_services::NativeWorkerBudget;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn parallel_quota_rejection_starts_no_closure() {
        static PROCESS: AtomicUsize = AtomicUsize::new(0);
        let dir = tempfile::tempdir().unwrap();
        let mut evaluator = Evaluator::new(dir.path().to_path_buf());
        Arc::make_mut(&mut evaluator.host).native_workers =
            NativeWorkerBudget::with_limits(1, &PROCESS, 8);
        let program = shoal_syntax::parse(
            r#"parallel(() => "first".save("one"), () => "second".save("two"))"#,
        )
        .unwrap();

        let error = evaluator.eval_program(&program).unwrap_err();
        assert_eq!(error.code, "session_worker_limit");
        assert!(!dir.path().join("one").exists());
        assert!(!dir.path().join("two").exists());
        assert_eq!(PROCESS.load(std::sync::atomic::Ordering::Relaxed), 0);
    }
}
