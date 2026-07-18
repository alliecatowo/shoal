//! Command evaluation: `eval_command`'s big dispatch (session callables,
//! bound-name-as-value, builtin heads, adapters, external spawn + redirects),
//! adapter argv construction, and the exec-spawning core (`run_argv`).

use super::*;
use crate::coerce::{signature, validate_adapter_value};
use crate::host::builtin_outcome;
use shoal_syntax::commands::CommandSource;

mod adapter;
mod capture;
mod external;
mod navigation;
mod redirects;
mod resolution;

pub(crate) use redirects::PreparedRedirects;

impl Evaluator {
    pub(crate) fn eval_command(&mut self, call: &CmdCall, position: Position) -> VResult<Value> {
        // Parser-authored calls already enforce this, but aliases, plugins and
        // embedders can construct CmdCall values directly. Validate before
        // background desugaring, argument evaluation, filesystem mutation, or
        // process spawn so ambiguous redirects can never partially execute.
        validate_redirect_shape(call)?;
        let resolution = self.resolve_command(call);
        // Builtin help is a canonical, zero-effect dispatch. Resolve first so
        // an in-session callable named `ls` still wins over the builtin, then
        // intercept before background desugaring, argument/glob evaluation,
        // env-prefix handling, redirects, Reef, filesystem, or process ports.
        if matches!(
            resolution.source,
            CommandSource::StructuredBuiltin | CommandSource::SpecialBuiltin
        ) && call_requests_help(call)
        {
            let help = shoal_syntax::commands::builtin_help(&call.head)
                .expect("every resolved builtin has canonical help metadata");
            self.emit(&Value::Str(help));
            return Ok(Value::Null);
        }
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
        if resolution.source == CommandSource::SessionCallable {
            let bound = resolution
                .binding
                .clone()
                .expect("callable resolution carries its binding");
            // `deploy --help` synthesises the signature + doc (site/content/internals/language-conformance-contract.md, defect #12).
            if let Value::Closure(c) = &bound
                && call
                    .args
                    .iter()
                    .any(|a| matches!(a, CmdArg::FlagLong { name, .. } if name == "help"))
            {
                let help = crate::helpers::closure_help(c.as_ref());
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
                    // item at the shared closure-call boundary.
                    CmdArg::Glob { .. } | CmdArg::Word { .. } | CmdArg::Path { .. }
                        if closure_sig
                            .and_then(|(params, _)| params.get(pos.len()))
                            .and_then(|p| p.ty.as_ref())
                            .is_some_and(|t| t.name == "list") =>
                    {
                        let items = self.expand_arg(a)?;
                        pos.push(Value::List(items));
                    }
                    _ => pos.extend(self.expand_arg(a)?),
                }
                i += 1;
            }
            return self.call_value(&bound, CallArgs { pos, named });
        }
        // A bare word bound to a non-callable value (e.g. `it`, `out`, or any
        // `let`) resolves to that value — bound names dispatch as EXPR (site/content/internals/language-conformance-contract.md).
        if resolution.source == CommandSource::BoundValue {
            return Ok(resolution
                .binding
                .expect("value resolution carries its binding"));
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
            self.exec.control.pending_exit = Some(code);
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
            let redirects = self.prepare_redirects(call, false)?;
            let value = self.builtin_which(call)?;
            let outcome = builtin_outcome("which", value);
            return self.apply_builtin_redirects(&redirects, outcome);
        }
        // `reef` builtin family (site/content/internals/reef-resolution.md): binding table, add, lock, fetch.
        if call.head == "reef" {
            let redirects = self.prepare_redirects(call, false)?;
            let value = self.builtin_reef(call)?;
            let outcome = builtin_outcome("reef", value);
            return self.apply_builtin_redirects(&redirects, outcome);
        }
        if call.head == "undo" {
            return self.builtin_undo(call);
        }
        if call.head == "journal" || call.head == "history" {
            return self.builtin_journal_view(call);
        }
        if resolution.source == CommandSource::StructuredBuiltin {
            // Outcome unification (P1a): a builtin yields a `Value::Outcome`
            // exactly like an external command — its structured result becomes
            // the outcome's `.out` (`parsed`), `status = 0`/`ok = true`. A
            // builtin error still raises as before (via `?`).
            //
            // site/content/internals/language-conformance-contract.md undo: capture prior state of an overwriting cp/mv/save
            // BEFORE the mutation, then record the typed inverse AFTER. All a
            // no-op unless a journal is installed and a statement is executing.
            let redirects = self.prepare_redirects(call, false)?;
            let undo_pre = self.fs_undo_pre(&call.head, call);
            let value = builtins::run(self, call)?;
            self.fs_undo_post(&call.head, undo_pre, &value);
            let outcome = builtin_outcome(&call.head, value);
            // Redirects apply to builtin results too (defect #8).
            return self.apply_builtin_redirects(&redirects, outcome);
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
            return Ok(Value::Path(self.exec.shell.cwd.clone()));
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
        if call.head == "source" || resolution.source == CommandSource::Script {
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
                self.exec.shell.cwd.join(script_path)
            };
            if is_source {
                let src = self.read_shoal_source(&path, "script")?;
                let program = shoal_syntax::parse_with_ctx(&src, self.parse_context(false))
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
        // `^name` bypasses plugins and adapters: only an unforced head can
        // enter an extension-owned command surface.
        if resolution.source == CommandSource::Plugin {
            return self.eval_wasm_command(call);
        }
        if resolution.source == CommandSource::Adapter {
            let redirects = self.prepare_redirects(call, true)?;
            let value = self.eval_adapter(call, position, &redirects)?;
            let value = self.apply_external_redirects(&redirects, value)?;
            return self.enforce_command_position(value, position, false);
        }
        debug_assert_eq!(resolution.source, CommandSource::External);
        let redirects = self.prepare_redirects(call, true)?;
        let mut argv = crate::args::ArgvBuilder::new(OsString::from(&call.head))?;
        for a in &call.args {
            for v in self.expand_arg(a)? {
                argv.push(self.argv_value(v)?)?;
            }
        }
        let argv = argv.finish();
        let stdin = redirects.stdin_spec();
        let output_redirected = redirects.has_output();
        // A redirected command must finish capture and commit its bytes before
        // statement-position failure promotion. This matches shell ordering:
        // `false > file` still creates/truncates `file`, then reports failure.
        let run_position = if output_redirected {
            Position::Value
        } else {
            position
        };
        let value = if output_redirected {
            self.run_argv_redirected(argv, run_position, stdin, &call.env_prefix, call.span, None)?
        } else {
            self.run_argv(argv, run_position, stdin, &call.env_prefix, call.span, None)?
        };
        let value = self.apply_external_redirects(&redirects, value)?;
        self.enforce_command_position(value, position, false)
    }

    /// Commit output redirects for any process-backed command (raw external or
    /// adapter) from the outcome's complete stdout, including lazy CAS spills.
    fn apply_external_redirects(
        &mut self,
        redirects: &PreparedRedirects,
        value: Value,
    ) -> VResult<Value> {
        let Value::Outcome(out) = &value else {
            return Ok(value);
        };
        let fs = self.host.fs.clone();
        if let Some(output) = redirects.output() {
            match output.kind {
                // Undo (site/content/internals/language-conformance-contract.md): an external command's `> file` / `>> file`
                // clobbers the target's contents just like `cp` — snapshot the
                // prior bytes first, record the restore inverse after, so
                // `some-cmd > f` and `sh { … } > f` are reversible too.
                RedirectKind::Out => {
                    let target = output.path.clone();
                    let undo_pre = self.redirect_undo_pre(&target);
                    let mut reader = out.open_stdout()?;
                    let mut writer = fs
                        .open_write(&target)
                        .map_err(|e| ErrorVal::new("custom", e.to_string()))?;
                    std::io::copy(&mut reader, &mut writer)
                        .map_err(|e| ErrorVal::new("custom", e.to_string()))?;
                    self.overwrite_undo_post(undo_pre);
                }
                RedirectKind::Append => {
                    let target = output.path.clone();
                    let undo_pre = self.redirect_undo_pre(&target);
                    let mut reader = out.open_stdout()?;
                    let mut writer = fs
                        .open_append(&target)
                        .map_err(|e| ErrorVal::new("custom", e.to_string()))?;
                    std::io::copy(&mut reader, &mut writer)
                        .map_err(|e| ErrorVal::new("custom", e.to_string()))?;
                    self.overwrite_undo_post(undo_pre);
                }
                RedirectKind::In => {}
            }
        }
        Ok(value)
    }
}

/// Does this command request standard help before the `--` end-of-flags
/// marker? Kept on parsed arguments so quoted/path values named `--help` never
/// acquire option semantics.
pub(crate) fn call_requests_help(call: &CmdCall) -> bool {
    call.args
        .iter()
        .take_while(|arg| !matches!(arg, CmdArg::DashDash { .. }))
        .any(|arg| match arg {
            CmdArg::FlagLong { name, .. } => name == "help",
            CmdArg::FlagShort { chars, .. } => chars.contains('h'),
            _ => false,
        })
}

fn validate_redirect_shape(call: &CmdCall) -> VResult<()> {
    let mut input = false;
    let mut output = false;
    for redirect in &call.redirects {
        let duplicate = match redirect.kind {
            RedirectKind::In => std::mem::replace(&mut input, true),
            RedirectKind::Out | RedirectKind::Append => std::mem::replace(&mut output, true),
        };
        if duplicate {
            let stream = if redirect.kind == RedirectKind::In {
                "stdin"
            } else {
                "stdout"
            };
            return Err(ErrorVal::arg_error(format!(
                "a command may have only one {stream} redirect"
            ))
            .with_span(redirect.span));
        }
    }
    Ok(())
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
    fn programmatic_calls_reject_ambiguous_redirects_before_dispatch() {
        let target = |name: &str, start: usize| Redirect {
            kind: RedirectKind::Out,
            target: CmdArg::Word {
                text: name.into(),
                span: Span::new(start, start + name.len()),
            },
            span: Span::new(start, start + name.len()),
        };
        let call = CmdCall {
            head: "definitely-not-spawned".into(),
            forced: false,
            args: Vec::new(),
            redirects: vec![target("first", 10), target("second", 20)],
            env_prefix: Vec::new(),
            background: true,
            trailing: None,
            span: Span::new(0, 26),
        };
        let error = validate_redirect_shape(&call).unwrap_err();
        assert_eq!(error.code, "arg_error");
        assert!(error.msg.contains("only one stdout redirect"));
        assert_eq!(error.span, Some(Span::new(20, 26)));
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
