//! External-process spawning, policy gating, and outcome construction.

use super::*;
use std::io::Read as _;

#[derive(Clone, Copy)]
enum ProcessMode {
    Auto(Position),
    Redirected(Position),
}

/// Ref-backed command values must stay comfortably below the lexical
/// environment's per-binding retained-value ceiling. The CAS owns the complete
/// bytes; this prefix is presentation only.
const CAPTURE_REF_PREVIEW_BYTES: usize = 1024 * 1024;

fn bound_ref_backed_preview(stdout: &mut Vec<u8>, has_spill: bool) {
    if has_spill && stdout.len() > CAPTURE_REF_PREVIEW_BYTES {
        stdout.truncate(CAPTURE_REF_PREVIEW_BYTES);
        stdout.shrink_to_fit();
    }
}

fn parse_adapter_output(meta: &ExecMeta, result: &shoal_exec::ExecResult) -> Option<Value> {
    // `stdout` is only a preview after a spill and can also be a truncated
    // prefix when capture hit its resident cap. A prefix that happens to end
    // on a row boundary must never masquerade as the command's complete
    // structured result.
    if !result.stdout_is_complete() {
        return None;
    }
    shoal_adapters::parse_output(&meta.parse, &result.stdout, meta.output_type.as_deref())
}

impl Evaluator {
    pub(crate) fn run_argv(
        &mut self,
        argv: Vec<OsString>,
        position: Position,
        stdin: StdinSpec,
        prefixes: &[EnvPrefix],
        span: Span,
        meta: Option<ExecMeta>,
    ) -> VResult<Value> {
        self.run_argv_inner(
            argv,
            ProcessMode::Auto(position),
            stdin,
            prefixes,
            span,
            meta,
        )
    }

    /// Run a command that has an output redirect. Redirected execution must
    /// capture even in an interactive statement: streaming through a PTY would
    /// both leak bytes to the terminal and retain only the bounded tee prefix.
    pub(super) fn run_argv_redirected(
        &mut self,
        argv: Vec<OsString>,
        position: Position,
        stdin: StdinSpec,
        prefixes: &[EnvPrefix],
        span: Span,
        meta: Option<ExecMeta>,
    ) -> VResult<Value> {
        self.run_argv_inner(
            argv,
            ProcessMode::Redirected(position),
            stdin,
            prefixes,
            span,
            meta,
        )
    }

    fn run_argv_inner(
        &mut self,
        mut argv: Vec<OsString>,
        process_mode: ProcessMode,
        stdin: StdinSpec,
        prefixes: &[EnvPrefix],
        span: Span,
        meta: Option<ExecMeta>,
    ) -> VResult<Value> {
        let (position, force_capture) = match process_mode {
            ProcessMode::Auto(position) => (position, false),
            ProcessMode::Redirected(position) => (position, true),
        };
        let mut env = self.exec.shell.process_env.clone();
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
        // A PTY has no portable input half-close: after finite bytes, a file,
        // or a stream have been written, merely dropping the master writer
        // cannot deliver EOF because the output reader still owns the master.
        // VEOF injection is line-discipline-specific and becomes literal data
        // in raw mode. Use a real stdin pipe for every finite input source so
        // `.feed` and `< file` terminate correctly even in statement position.
        let finite_stdin = matches!(
            &stdin,
            StdinSpec::Bytes(_) | StdinSpec::File(_) | StdinSpec::Stream(_)
        );
        let mode = if !force_capture
            && !finite_stdin
            && (force_tui || (self.session.interactive && position == Position::Statement))
        {
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
                    cwd: self.exec.shell.cwd.clone(),
                    env,
                    stdin,
                    mode,
                    sandbox,
                    spill,
                },
                &self.exec.control.cancel,
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
        let parsed = meta
            .as_ref()
            .and_then(|meta| parse_adapter_output(meta, &r));
        // A capture spill already preserves the complete stream in the CAS.
        // Retaining the executor's much larger transient preview in every
        // lexical binding would defeat that indirection and can exceed the
        // environment's aggregate retained-value budget.
        bound_ref_backed_preview(&mut r.stdout, r.stdout_spill.is_some());
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
        self.enforce_command_position(out, position)
    }

    /// Promote an unsuccessful outcome to Shoal's statement-position
    /// `cmd_failed` error while leaving value-position outcomes inspectable.
    /// Redirect callers run in value position until bytes have been committed,
    /// then apply the original position through this shared gate.
    pub(super) fn enforce_command_position(
        &self,
        out: Value,
        position: Position,
    ) -> VResult<Value> {
        if position == Position::Statement
            && let Value::Outcome(failed) = &out
            && !failed.ok
        {
            let message = match (failed.status, failed.signal.as_deref()) {
                (Some(code), _) => format!("`{}` exited with status {code}", failed.cmd),
                (_, Some(signal)) => format!("`{}` died from {signal}", failed.cmd),
                _ => format!("`{}` failed", failed.cmd),
            };
            return Err(ErrorVal::new("cmd_failed", message)
                .with_status(failed.status)
                .with_stderr(String::from_utf8_lossy(&failed.stderr).into_owned()));
        }
        Ok(out)
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
    /// port in fixed chunks so policy preflight cannot retain an executable-
    /// sized allocation.
    pub(crate) fn hash_resolved_bin(&self, argv0: &OsStr) -> Option<String> {
        let candidate = Path::new(argv0);
        let resolved = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.ambient_which(&argv0.to_string_lossy())?
        };
        if !self.host.fs.metadata(&resolved).ok()?.is_file() {
            return None;
        }
        let mut file = self.host.fs.open_read(&resolved).ok()?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = file.read(&mut buffer).ok()?;
            if count == 0 {
                break;
            }
            hasher.update(&buffer[..count]);
        }
        Some(hasher.finalize().to_hex().to_string())
    }
}

#[cfg(test)]
mod adapter_output_boundary_tests {
    use super::*;

    fn result(stdout: &[u8]) -> shoal_exec::ExecResult {
        shoal_exec::ExecResult {
            status: Some(0),
            signal: None,
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
            truncated: false,
            stdout_spill: None,
            dur: std::time::Duration::ZERO,
            pid: 1,
            pgid: 1,
            stopped: false,
            enforcement: None,
        }
    }

    fn meta() -> ExecMeta {
        ExecMeta {
            ok_codes: vec![0],
            class: AdapterClass::Cli,
            parse: "ndjson".into(),
            output_type: None,
        }
    }

    #[test]
    fn only_complete_resident_stdout_can_be_structured() {
        let mut output = result(b"{\"a\":1}\n");
        assert!(matches!(
            parse_adapter_output(&meta(), &output),
            Some(Value::Table(rows)) if rows.len() == 1
        ));

        output.truncated = true;
        assert_eq!(parse_adapter_output(&meta(), &output), None);
    }

    #[test]
    fn ref_backed_preview_is_reduced_before_lexical_retention() {
        let mut stdout = vec![b'x'; CAPTURE_REF_PREVIEW_BYTES * 2];
        bound_ref_backed_preview(&mut stdout, true);
        assert_eq!(stdout.len(), CAPTURE_REF_PREVIEW_BYTES);
        assert_eq!(stdout.capacity(), CAPTURE_REF_PREVIEW_BYTES);

        let mut resident = vec![b'x'; CAPTURE_REF_PREVIEW_BYTES + 1];
        bound_ref_backed_preview(&mut resident, false);
        assert_eq!(resident.len(), CAPTURE_REF_PREVIEW_BYTES + 1);
    }
}
