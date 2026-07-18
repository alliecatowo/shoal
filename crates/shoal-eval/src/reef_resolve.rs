//! reef scope-chain cache, override stack, and spawn-time resolution
//! (site/content/internals/reef-resolution.md).
//!
//! Split out of [`crate::reef`] (see that module's doc for the split
//! rationale): this file owns the cached [`ScopeChain`]/lock, the
//! `with reef:` override stack, and the `reef_apply` spawn hook.
//! [`crate::reef_builtins`] owns the user-facing `which`/`reef` commands built
//! on top of this.

use super::*;

use shoal_reef::provider::{ProviderCommand, ProviderCtx, ProviderRunner};
use shoal_reef::{
    Binding, Candidate, LockNotice, ManifestKind, Policy, ProbeExecution, ReefError, ReefResult,
    Resolver, ScopeChain, ScopeEntry, ViewConfig, default_view_root, synth_path,
};

struct EvaluatorProviderRunner {
    exec: Arc<dyn Exec>,
    env: Vec<(OsString, OsString)>,
    sandbox: Option<SandboxPolicy>,
    cancel: CancelToken,
}

impl ProviderRunner for EvaluatorProviderRunner {
    fn run(
        &self,
        command: ProviderCommand<'_>,
    ) -> std::io::Result<shoal_exec::BoundedCommandOutput> {
        let mut argv = Vec::with_capacity(command.args.len() + 1);
        argv.push(command.program.as_os_str().to_owned());
        argv.extend(command.args.iter().cloned());
        self.exec.run_bounded(
            ExecSpec {
                argv,
                cwd: command.cwd.to_path_buf(),
                env: self.env.clone(),
                stdin: StdinSpec::Null,
                mode: ExecMode::Capture,
                sandbox: self.sandbox.clone(),
                spill: None,
            },
            command.timeout,
            command.output_cap,
            &self.cancel,
        )
    }
}

impl Evaluator {
    /// Fail closed before Reef executes an unknown-version candidate as
    /// `<candidate> --version`. Probes are opaque code execution: a restricted
    /// principal must allow opaque effects and any active spawn pin. Execution
    /// happens separately through [`EvaluatorProviderRunner`], which carries
    /// the active filesystem sandbox and cancellation epoch.
    pub(crate) fn reef_probe_guard(&self, candidate: &Candidate) -> ReefResult<()> {
        let Some((policy, principal)) = self.session.leash.as_ref() else {
            return Ok(());
        };
        if policy.evaluate_effect(principal, &Effect::Opaque) != shoal_leash::Verdict::Allow {
            return Err(ReefError::provider(format!(
                "version probe for {} denied: principal `{principal}` does not allow opaque effects",
                candidate.path.display()
            ))
            .with_hint("lock the tool in an unrestricted trusted session before using it here"));
        }
        if policy.spawn_pinning_active(principal) {
            let effect = Effect::ProcSpawn {
                bin_hash: self
                    .hash_resolved_bin(candidate.path.as_os_str())
                    .unwrap_or_default(),
                argv0: candidate.path.to_string_lossy().into_owned(),
            };
            if policy.evaluate_effect(principal, &effect) != shoal_leash::Verdict::Allow {
                return Err(ReefError::provider(format!(
                    "version probe for {} denied by principal `{principal}` spawn pins",
                    candidate.path.display()
                ))
                .with_hint("allow the candidate name/hash or materialize the lock elsewhere"));
            }
        }
        Ok(())
    }

    /// `fetch` is an explicit but opaque installer spawn. Enforce the active
    /// evaluator policy again at execution time so embedded/non-kernel hosts
    /// cannot bypass the plan verdict. Provider execution separately carries
    /// the active filesystem sandbox and cancellation epoch.
    pub(crate) fn reef_fetch_guard(&self) -> VResult<()> {
        let Some((policy, principal)) = self.session.leash.as_ref() else {
            return Ok(());
        };
        if policy.evaluate_effect(principal, &Effect::Opaque) != shoal_leash::Verdict::Allow {
            return Err(ErrorVal::new(
                "spawn_denied",
                format!("reef fetch denied: principal `{principal}` does not allow opaque effects"),
            ));
        }
        if policy.spawn_pinning_active(principal) {
            let mise = self.ambient_which("mise").ok_or_else(|| {
                ErrorVal::new(
                    "spawn_denied",
                    "reef fetch denied: no `mise` executable is available for spawn-pin verification",
                )
            })?;
            self.spawn_gate(mise.as_os_str(), None, Span::default())?;
        }
        Ok(())
    }

    pub(crate) fn reef_provider_context(&self, cwd: PathBuf) -> ProviderCtx {
        let path_env = self
            .exec
            .shell
            .process_env
            .iter()
            .find(|(name, _)| name == "PATH")
            .map(|(_, value)| value.clone());
        ProviderCtx::with_runner(
            cwd,
            path_env,
            Arc::new(EvaluatorProviderRunner {
                exec: self.host.exec.clone(),
                env: self.exec.shell.process_env.clone(),
                sandbox: self.resolve_sandbox(),
                cancel: self.exec.control.cancel.clone(),
            }),
        )
    }

    // --- chain cache -------------------------------------------------------

    /// Ensure the cached scope chain matches the current cwd and manifest
    /// metadata. A new, edited, removed, or replaced candidate invalidates the
    /// cache without requiring a directory change. Reloads the adjacent lock at
    /// the same time.
    pub(crate) fn ensure_reef_chain(&mut self) {
        let observed_key = ScopeChain::discovery_key_with(
            &self.exec.shell.cwd,
            self.host.reef_user_manifest.as_deref(),
            self.host.fs.as_ref(),
        );
        let fresh = match &self.exec.reef.chain {
            Some((cwd, _)) => {
                cwd != &self.exec.shell.cwd
                    || self.exec.reef.chain_key.as_ref() != Some(&observed_key)
            }
            None => true,
        };
        if !fresh {
            return;
        }
        let chain = ScopeChain::discover_with(
            &self.exec.shell.cwd,
            self.host.reef_user_manifest.as_deref(),
            self.host.fs.as_ref(),
        );
        let warnings = chain.warnings.clone();
        self.exec.reef.discovery_error = if self.session.interactive || warnings.is_empty() {
            None
        } else {
            Some(format!(
                "Reef discovery retained {} warning(s) for invalid or unreadable manifests; first: {}",
                warnings.len(),
                warnings[0]
            ))
        };
        self.exec.reef.lock_path = chain
            .scopes
            .iter()
            .find(|s| s.kind == ManifestKind::Reef)
            .or_else(|| chain.scopes.first())
            .map(|s| shoal_reef::Lockfile::path_next_to(&s.source));
        let loaded = self
            .exec
            .reef
            .lock_path
            .as_ref()
            .map(|path| shoal_reef::Lockfile::load_with(path, self.host.fs.as_ref()));
        match loaded {
            Some(Ok(lock)) => {
                self.exec.reef.lock = lock;
                self.exec.reef.lock_load_error = None;
            }
            Some(Err(error)) => {
                self.exec.reef.lock = shoal_reef::Lockfile::new();
                self.exec.reef.lock_load_error = Some(error.to_string());
            }
            None => {
                self.exec.reef.lock = shoal_reef::Lockfile::new();
                self.exec.reef.lock_load_error = None;
            }
        }
        self.exec.reef.chain = Some((self.exec.shell.cwd.clone(), chain));
        self.exec.reef.chain_key = Some(observed_key);
        if self.session.interactive {
            for warning in warnings.iter().take(8) {
                self.emit_line(&format!("reef: warning: {warning}"));
            }
            if warnings.len() > 8 {
                self.emit_line(&format!(
                    "reef: warning: {} additional discovery warning(s)",
                    warnings.len() - 8
                ));
            }
        }
    }

    /// A clone of the current scope chain (cheap: manifests are small maps),
    /// with any active `with reef:` override layers (site/content/internals/reef-resolution.md) prepended —
    /// nearest-first, so the innermost `with reef:` block wins ties, then the
    /// discovered manifest chain. The clone frees `self` for the resolver/lock
    /// mutations that follow, and never mutates the cached `reef_chain` (so
    /// popping an override always restores exactly the cached chain).
    pub(crate) fn reef_chain_snapshot(&mut self) -> ScopeChain {
        self.ensure_reef_chain();
        let mut chain = self
            .exec
            .reef
            .chain
            .as_ref()
            .expect("just ensured")
            .1
            .clone();
        if !self.exec.reef.overrides.is_empty() {
            let mut scopes: Vec<ScopeEntry> =
                self.exec.reef.overrides.iter().rev().cloned().collect();
            scopes.append(&mut chain.scopes);
            chain.scopes = scopes;
        }
        chain
    }

    /// Push a `with reef: {tool: constraint, …}` override layer for the
    /// dynamic extent of a block (site/content/internals/reef-resolution.md), minted from a plain record of
    /// tool name -> version-constraint string. Highest priority: it out-ranks
    /// every discovered manifest and every previously-pushed override while
    /// active. Pop with [`Evaluator::pop_reef_override`] on every exit path.
    pub(crate) fn push_reef_override(&mut self, record: &Record) -> VResult<()> {
        let mut tools = std::collections::BTreeMap::new();
        for (k, v) in record {
            let Value::Str(s) = v else {
                return Err(ErrorVal::type_error(format!(
                    "with reef: expects {{tool: \"constraint\"}}, found {} for `{k}`",
                    v.type_name()
                )));
            };
            tools.insert(
                k.clone(),
                shoal_reef::ToolReq::new(shoal_reef::Constraint::parse(s)),
            );
        }
        self.exec.reef.overrides.push(ScopeEntry {
            kind: ManifestKind::Reef,
            source: PathBuf::from("<with reef:>"),
            manifest: shoal_reef::ReefManifest {
                tools,
                runners: Default::default(),
                hermetic: false,
            },
            mtime: None,
        });
        Ok(())
    }

    /// Pop the most recently pushed `with reef:` override layer. A no-op past
    /// the bottom of the stack (defensive; callers always balance push/pop).
    pub(crate) fn pop_reef_override(&mut self) {
        self.exec.reef.overrides.pop();
    }

    /// The lazily-built provider stack (site/content/internals/reef-resolution.md). Only ever called once a
    /// manifest is in scope, so the no-manifest hot path never constructs it.
    pub(crate) fn reef_resolver(&self) -> Arc<Resolver> {
        self.host
            .reef_resolver
            .get_or_init(|| Arc::new(Resolver::with_defaults()))
            .clone()
    }

    /// True when at least one manifest — discovered or a `with reef:`
    /// override — constrains something in the current scope. The single gate
    /// that keeps the no-manifest world untouched.
    pub(crate) fn reef_manifest_in_scope(&mut self) -> bool {
        if !self.exec.reef.overrides.is_empty() {
            return true;
        }
        self.ensure_reef_chain();
        !self
            .exec
            .reef
            .chain
            .as_ref()
            .expect("ensured")
            .1
            .scopes
            .is_empty()
    }

    /// Persist a candidate lock next to its manifest before publishing it as
    /// evaluator state. A constrained resolution is not durably locked until
    /// this succeeds.
    pub(crate) fn persist_reef_lock_value(&self, lock: &shoal_reef::Lockfile) -> VResult<()> {
        self.reef_lock_loaded()?;
        let path = self.exec.reef.lock_path.as_ref().ok_or_else(|| {
            ErrorVal::new(
                "reef_provider",
                "cannot persist Reef lock: no manifest-backed lockfile target",
            )
        })?;
        lock.save_with(path, self.host.fs.as_ref())
            .map_err(|error| {
                ErrorVal::new(
                    "reef_provider",
                    format!("persisting Reef lock {}: {error}", path.display()),
                )
            })
    }

    pub(crate) fn reef_lock_loaded(&self) -> VResult<()> {
        match &self.exec.reef.lock_load_error {
            Some(error) => Err(ErrorVal::new(
                "reef_provider",
                format!("cannot use malformed Reef lock: {error}"),
            )
            .with_hint("inspect or remove reef.lock, then run `reef lock --refresh`")),
            None => Ok(()),
        }
    }

    /// Look up `name` on the ambient `$PATH`, bypassing reef entirely — the
    /// same raw lookup `which`'s NotFound fallback already performs. Shared by
    /// the not-found "shadowed by ambient PATH" did-you-mean (below) and
    /// `reef doctor`'s shadowed-ambient check (`reef_builtins.rs`, site/content/internals/reef-resolution.md
    /// third bullet): both need the same "does a name answer to something
    /// outside reef's view" fact.
    pub(crate) fn ambient_which(&self, name: &str) -> Option<PathBuf> {
        let path_env = self
            .exec
            .shell
            .process_env
            .iter()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| v.as_os_str());
        shoal_exec::which_in(OsStr::new(name), path_env, &self.exec.shell.cwd)
    }

    // --- spawn-time resolution (site/content/internals/reef-resolution.md) -------------------------------

    /// The reef spawn hook, called from `run_argv` just before spawning. When
    /// the head (`argv[0]`, a bare name) is constrained by a manifest in scope,
    /// rewrites `argv[0]` to the resolved absolute binary and rewrites the
    /// child's `PATH` to a synthesized view (site/content/internals/reef-resolution.md). When nothing is in scope
    /// or the head is unconstrained, it is a pure no-op — today's behavior.
    ///
    /// `env` is the child environment being assembled; only its `PATH` entry is
    /// ever touched, and only for a constrained spawn. The session env is never
    /// mutated.
    ///
    /// Returns the resolved binary's blake3 content hash (`Some`) when reef
    /// actually resolved the head, so the leash spawn gate (site/content/internals/language-conformance-contract.md content-hash
    /// pinning) can reuse it verbatim instead of re-hashing the same file;
    /// `None` on every no-op path (no manifest, explicit path, unconstrained
    /// head). The hash is reef's own `Resolution::hash`, identical blake3-hex to
    /// `shoal_leash::preflight_spawn`, so a pin an author copies from `reef`
    /// output compares equal either way.
    pub(crate) fn reef_apply(
        &mut self,
        argv: &mut [OsString],
        env: &mut Vec<(OsString, OsString)>,
        span: Span,
    ) -> VResult<Option<String>> {
        self.ensure_reef_chain();
        if let Some(error) = &self.exec.reef.discovery_error {
            return Err(ErrorVal::new("reef_provider", error.clone())
                .with_hint(
                    "fix or remove the reported manifest before running a script/agent command",
                )
                .with_span(span));
        }
        // Fast bail: no manifest in scope ⇒ never touch the resolver.
        if !self.reef_manifest_in_scope() {
            return Ok(None);
        }
        let Some(head) = argv.first() else {
            return Ok(None);
        };
        // An explicit path bypasses name resolution (session fn/alias → adapter
        // bin pin → reef → …; a `/`-bearing argv[0] is already a bound binary).
        let name = head.to_string_lossy().into_owned();
        if name.contains('/') {
            return Ok(None);
        }
        let chain = self.reef_chain_snapshot();
        if chain.nearest_for(&name).is_none() {
            // Manifest in scope, but it does not mention this tool ⇒ exactly
            // today's behavior: ambient PATH, PATH/which resolution, untouched.
            return Ok(None);
        }
        self.reef_lock_loaded()
            .map_err(|error| error.with_span(span))?;

        let policy = if self.session.interactive {
            Policy::Interactive
        } else {
            Policy::Script
        };
        let resolver = self.reef_resolver();
        let mut lock = self.exec.reef.lock.clone();
        let mut notice: Option<LockNotice> = None;
        let provider_context = self.reef_provider_context(chain.cwd.clone());
        let outcome = resolver.resolve_with_probe_context(
            &name,
            &chain,
            &mut lock,
            policy,
            &mut |n| notice = Some(n.clone()),
            ProbeExecution {
                guard: &mut |candidate| self.reef_probe_guard(candidate),
                context: &provider_context,
            },
        );
        let resolution = match outcome {
            Ok(r) => r,
            Err(e) => {
                // site/content/internals/reef-resolution.md second did-you-mean bullet: a constrained,
                // not-found tool might still answer to a DIFFERENT binary via
                // plain ambient PATH — surface that so the miss doesn't read
                // as "nothing anywhere has this" when ambient actually does,
                // just shadowed by the project's reef scope.
                let ambient = self.ambient_which(&name);
                return Err(reef_error_to_val(e, &name, &chain, ambient.as_deref()).with_span(span));
            }
        };

        if notice.is_some() {
            self.persist_reef_lock_value(&lock)
                .map_err(|error| error.with_span(span))?;
        }

        argv[0] = resolution.path.clone().into_os_string();
        self.exec.reef.lock = lock;
        if let Some(n) = notice {
            self.emit_lock_notice(&n);
        }

        // Synthesize the child's PATH so legacy children see a coherent world
        // (site/content/internals/reef-resolution.md): the reef view dir first, then the ambient PATH tail unless
        // a scope requested hermetic. Never mutates the session env.
        let path_var = self.reef_synth_path(&resolution, &chain, env)?;
        match env.iter_mut().find(|(k, _)| k == "PATH") {
            Some(pair) => pair.1 = path_var,
            None => env.push((OsString::from("PATH"), path_var)),
        }
        Ok(Some(resolution.hash))
    }

    /// Build (or reuse) a content-addressed view dir binding every locked tool,
    /// and return the synthesized `PATH` value (site/content/internals/reef-resolution.md). The system tail is the
    /// child's *ambient* PATH (so non-reef tools still resolve), dropped entirely
    /// when hermetic.
    fn reef_synth_path(
        &self,
        resolution: &shoal_reef::Resolution,
        chain: &ScopeChain,
        env: &[(OsString, OsString)],
    ) -> VResult<OsString> {
        let mut bindings = vec![Binding::new(
            resolution.report.name.clone(),
            resolution.path.clone(),
        )];
        for (tool, entry) in &self.exec.reef.lock.tools {
            if tool != &resolution.report.name {
                bindings.push(Binding::new(tool.clone(), entry.path.clone()));
            }
        }
        let hermetic = chain.hermetic();
        let system_tail = if hermetic {
            Vec::new()
        } else {
            env.iter()
                .find(|(k, _)| k == "PATH")
                .map(|(_, v)| std::env::split_paths(v).collect::<Vec<_>>())
                .unwrap_or_default()
        };
        let cfg = ViewConfig {
            root: default_view_root(),
            system_tail,
            hermetic,
        };
        let view = synth_path(&bindings, &cfg)
            .map_err(|e| ErrorVal::new("reef_provider", format!("synthesizing PATH: {e}")))?;
        Ok(view.path_var)
    }

    /// Emit the one-line auto-lock notice to the statement sink (site/content/internals/reef-resolution.md).
    fn emit_lock_notice(&mut self, n: &LockNotice) {
        let msg = format!(
            "reef: locked {}@{} via {} ({})",
            n.name,
            n.version,
            n.provider,
            n.path.display()
        );
        self.emit_line(&msg);
    }

    /// Route a one-line diagnostic through the sink (or stderr without one).
    fn emit_line(&mut self, msg: &str) {
        if self.session.sink.is_some() {
            let v = Value::Str(msg.to_string());
            self.emit(&v);
        } else {
            eprintln!("{msg}");
        }
    }
}

/// Convert a [`shoal_reef::ReefError`] into an `ErrorVal`, preserving the stable
/// code and hint. Enriches `reef_not_found` on a constrained tool with the
/// did-you-mean phrasing from site/content/internals/reef-resolution.md: "constrained but not installed", plus
/// (when `ambient` names a real ambient-PATH hit for the same name) "found in
/// ambient PATH but shadowed by project reef" — site/content/internals/reef-resolution.md second bullet.
fn reef_error_to_val(
    e: shoal_reef::ReefError,
    name: &str,
    chain: &ScopeChain,
    ambient: Option<&Path>,
) -> ErrorVal {
    use shoal_reef::ReefCode;
    let (code, msg) = if e.code == ReefCode::NotFound {
        let constraint = chain
            .nearest_for(name)
            .map(|s| s.manifest.tools[name].constraint.to_string());
        match constraint {
            Some(c) => {
                let mut m =
                    format!("`{name}` is constrained ({c}) but not installed — reef fetch {name}");
                if let Some(p) = ambient {
                    m.push_str(&format!(
                        " (found in ambient PATH at {} but shadowed by project reef)",
                        p.display()
                    ));
                }
                (e.code_str(), m)
            }
            None => (e.code_str(), e.msg.clone()),
        }
    } else {
        (e.code_str(), e.msg.clone())
    };
    let mut out = ErrorVal::new(code, msg);
    if let Some(h) = e.hint {
        out = out.with_hint(h);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use shoal_reef::Resolver;
    use shoal_reef::provider::SystemProvider;

    /// A resolver whose only provider is a system provider rooted at a
    /// nonexistent dir with NO ambient dirs — it can never find a candidate
    /// for anything, no matter what the real `$PATH` holds. Mirrors
    /// `crates/shoal-eval/tests/reef_integration.rs`'s own `fixture_resolver`
    /// pattern, just deliberately empty instead of pointed at a fixture bin.
    fn empty_fixture_resolver() -> Arc<Resolver> {
        Arc::new(Resolver::new(vec![Box::new(SystemProvider::new(
            vec![PathBuf::from("/nonexistent-shoal-reef-fixture-root-9f3a")],
            vec![],
        ))]))
    }

    /// Fix 5 (site/content/internals/reef-resolution.md second did-you-mean bullet): a constrained tool
    /// the fixture resolver can't find, but that a REAL ambient binary
    /// answers to (here, `sh` — guaranteed present on any POSIX host, the
    /// same assumption `crates/shoal-eval/tests/reef_integration.rs` and much
    /// of this corpus already make), must name the shadowing in the spawn's
    /// `reef_not_found` error — not just "not installed".
    #[test]
    fn spawn_not_found_names_ambient_shadow_when_present() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".reef.toml"), "[tools]\nsh = \"*\"\n").unwrap();
        let mut ev = Evaluator::new(dir.path().to_path_buf());
        ev.set_interactive(true); // reach resolve_fresh, not reef_unlocked
        ev.set_reef_resolver(empty_fixture_resolver());

        let err = ev
            .eval_program(&shoal_syntax::parse("^sh").unwrap())
            .expect_err("the empty fixture resolver offers no `sh` candidate");
        assert_eq!(err.code, "reef_not_found");
        assert!(
            err.msg.contains("found in ambient PATH at"),
            "expected the ambient-shadow hint, got {:?}",
            err.msg
        );
        assert!(
            err.msg.contains("shadowed by project reef"),
            "expected the shadowed-by-reef phrasing, got {:?}",
            err.msg
        );
    }

    /// The hint must NOT appear when nothing — neither reef nor ambient PATH
    /// — actually has the tool: a genuinely absent tool stays exactly
    /// "constrained but not installed", no false-positive shadowing claim.
    #[test]
    fn spawn_not_found_omits_ambient_hint_when_truly_absent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".reef.toml"),
            "[tools]\nghosttool-shoal-corpus-9f3a = \"*\"\n",
        )
        .unwrap();
        let mut ev = Evaluator::new(dir.path().to_path_buf());
        ev.set_interactive(true);
        ev.set_reef_resolver(empty_fixture_resolver());

        let err = ev
            .eval_program(&shoal_syntax::parse("^ghosttool-shoal-corpus-9f3a").unwrap())
            .expect_err("nothing anywhere provides this tool");
        assert_eq!(err.code, "reef_not_found");
        assert!(
            !err.msg.contains("shadowed by project reef"),
            "no real ambient hit exists; the hint must not fire, got {:?}",
            err.msg
        );
    }

    #[test]
    fn noninteractive_discovery_fails_closed_and_recovers_after_manifest_fix() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = dir.path().join(".reef.toml");
        std::fs::write(&manifest, "[tools").unwrap();
        let mut evaluator = Evaluator::new(dir.path().to_path_buf());
        let program = shoal_syntax::parse("/bin/true").unwrap();

        let error = evaluator
            .eval_program(&program)
            .expect_err("script/agent execution must not skip malformed authority");
        assert_eq!(error.code, "reef_provider");
        assert!(error.msg.contains("invalid or unreadable manifests"));

        std::fs::write(&manifest, "").unwrap();
        let value = evaluator
            .eval_program(&program)
            .expect("same-cwd metadata identity must notice the repaired manifest");
        let Value::Outcome(outcome) = value else {
            panic!("expected external outcome");
        };
        assert!(outcome.ok);
        assert!(evaluator.exec.reef.discovery_error.is_none());
    }

    #[test]
    fn interactive_discovery_warns_once_and_remains_best_effort() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".reef.toml"), "[tools").unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let capture = seen.clone();
        let mut evaluator = Evaluator::new(dir.path().to_path_buf());
        evaluator.set_interactive(true);
        evaluator.set_statement_sink(Box::new(move |value| {
            if let Value::Str(line) = value {
                capture.lock().unwrap().push(line.clone());
            }
        }));

        evaluator.ensure_reef_chain();
        evaluator.ensure_reef_chain();
        assert!(evaluator.exec.reef.discovery_error.is_none());
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "an unchanged bad scope warns only once");
        assert!(seen[0].contains("reef: warning:"));
    }
}
