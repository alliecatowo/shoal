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
                    self.fs
                        .write(&p, &value_bytes(&value))
                        .map_err(|e| ErrorVal::new("custom", e.to_string()))?;
                    self.overwrite_undo_post(undo_pre);
                    captured = true;
                }
                RedirectKind::Append => {
                    let p = self.arg_path(&r.target)?;
                    let undo_pre = self.redirect_undo_pre(&p);
                    self.fs
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
        let p = if p.is_absolute() { p } else { self.cwd.join(p) };
        self.opener
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
        let mut handles = Vec::new();
        for f in a.pos {
            let env = self.env.clone();
            let cwd = self.cwd.clone();
            let penv = self.process_env.clone();
            let adapters = self.adapters.clone();
            // Share the host's effect ports (site/content/internals/roadmap-and-priorities.md) so a `parallel`
            // child spawned under a fake/custom adapter sees it too. Cheap `Arc`
            // clones; identical to the old behavior under the `Std*` defaults.
            let fs = self.fs.clone();
            let exec = self.exec.clone();
            let clock = self.clock.clone();
            let opener = self.opener.clone();
            let secrets = self.secrets.clone();
            handles.push(std::thread::spawn(move || {
                let mut ev = Evaluator::new(cwd);
                ev.env = env;
                ev.process_env = penv;
                ev.adapters = adapters;
                ev.fs = fs;
                ev.exec = exec;
                ev.clock = clock;
                ev.opener = opener;
                ev.secrets = secrets;
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
