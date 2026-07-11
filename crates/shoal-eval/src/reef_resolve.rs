//! reef scope-chain cache, override stack, and spawn-time resolution
//! (docs/REEF.md §2–§4, §6).
//!
//! Split out of [`crate::reef`] (see that module's doc for the split
//! rationale): this file owns the cached [`ScopeChain`]/lock, the
//! `with reef:` override stack, and the `reef_apply` spawn hook.
//! [`crate::reef_builtins`] owns the user-facing `which`/`reef` commands built
//! on top of this.

use super::*;

use shoal_reef::{
    Binding, LockNotice, ManifestKind, Policy, Resolver, ScopeChain, ScopeEntry, ViewConfig,
    default_view_root, synth_path,
};

impl Evaluator {
    // --- chain cache -------------------------------------------------------

    /// Ensure the cached scope chain matches the current cwd. Rebuilds only when
    /// the cwd changed since the last discovery (so `cd` / `with cwd:` re-scope
    /// the next resolution and nothing else does). Reloads the lock next to the
    /// nearest manifest at the same time.
    pub(crate) fn ensure_reef_chain(&mut self) {
        let fresh = match &self.reef_chain {
            Some((cwd, _)) => cwd != &self.cwd,
            None => true,
        };
        if !fresh {
            return;
        }
        let chain = ScopeChain::discover(&self.cwd, self.reef_user_manifest.as_deref());
        self.reef_lock_path = chain
            .scopes
            .iter()
            .find(|s| s.kind == ManifestKind::Reef)
            .or_else(|| chain.scopes.first())
            .map(|s| shoal_reef::Lockfile::path_next_to(&s.source));
        self.reef_lock = self
            .reef_lock_path
            .as_ref()
            .and_then(|p| shoal_reef::Lockfile::load(p).ok())
            .unwrap_or_default();
        self.reef_chain = Some((self.cwd.clone(), chain));
    }

    /// A clone of the current scope chain (cheap: manifests are small maps),
    /// with any active `with reef:` override layers (REEF.md §6) prepended —
    /// nearest-first, so the innermost `with reef:` block wins ties, then the
    /// discovered manifest chain. The clone frees `self` for the resolver/lock
    /// mutations that follow, and never mutates the cached `reef_chain` (so
    /// popping an override always restores exactly the cached chain).
    pub(crate) fn reef_chain_snapshot(&mut self) -> ScopeChain {
        self.ensure_reef_chain();
        let mut chain = self.reef_chain.as_ref().expect("just ensured").1.clone();
        if !self.reef_overrides.is_empty() {
            let mut scopes: Vec<ScopeEntry> = self.reef_overrides.iter().rev().cloned().collect();
            scopes.append(&mut chain.scopes);
            chain.scopes = scopes;
        }
        chain
    }

    /// Push a `with reef: {tool: constraint, …}` override layer for the
    /// dynamic extent of a block (REEF.md §6), minted from a plain record of
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
        self.reef_overrides.push(ScopeEntry {
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
        self.reef_overrides.pop();
    }

    /// The lazily-built provider stack (REEF §3). Only ever called once a
    /// manifest is in scope, so the no-manifest hot path never constructs it.
    pub(crate) fn reef_resolver(&mut self) -> Arc<Resolver> {
        if self.reef_resolver.is_none() {
            self.reef_resolver = Some(Arc::new(Resolver::with_defaults()));
        }
        self.reef_resolver.as_ref().expect("just set").clone()
    }

    /// True when at least one manifest — discovered or a `with reef:`
    /// override — constrains something in the current scope. The single gate
    /// that keeps the no-manifest world untouched.
    pub(crate) fn reef_manifest_in_scope(&mut self) -> bool {
        if !self.reef_overrides.is_empty() {
            return true;
        }
        self.ensure_reef_chain();
        !self
            .reef_chain
            .as_ref()
            .expect("ensured")
            .1
            .scopes
            .is_empty()
    }

    /// Persist the in-memory lock next to its manifest, best-effort. A failure
    /// to write never fails a spawn — the lock is an optimization, not a gate.
    pub(crate) fn persist_reef_lock(&self) {
        if let Some(path) = &self.reef_lock_path {
            let _ = self.reef_lock.save(path);
        }
    }

    // --- spawn-time resolution (REEF §2, §4) -------------------------------

    /// The reef spawn hook, called from `run_argv` just before spawning. When
    /// the head (`argv[0]`, a bare name) is constrained by a manifest in scope,
    /// rewrites `argv[0]` to the resolved absolute binary and rewrites the
    /// child's `PATH` to a synthesized view (REEF §4). When nothing is in scope
    /// or the head is unconstrained, it is a pure no-op — today's behavior.
    ///
    /// `env` is the child environment being assembled; only its `PATH` entry is
    /// ever touched, and only for a constrained spawn. The session env is never
    /// mutated.
    pub(crate) fn reef_apply(
        &mut self,
        argv: &mut [OsString],
        env: &mut Vec<(OsString, OsString)>,
        span: Span,
    ) -> VResult<()> {
        // Fast bail: no manifest in scope ⇒ never touch the resolver.
        if !self.reef_manifest_in_scope() {
            return Ok(());
        }
        let Some(head) = argv.first() else {
            return Ok(());
        };
        // An explicit path bypasses name resolution (session fn/alias → adapter
        // bin pin → reef → …; a `/`-bearing argv[0] is already a bound binary).
        let name = head.to_string_lossy().into_owned();
        if name.contains('/') {
            return Ok(());
        }
        let chain = self.reef_chain_snapshot();
        if chain.nearest_for(&name).is_none() {
            // Manifest in scope, but it does not mention this tool ⇒ exactly
            // today's behavior: ambient PATH, PATH/which resolution, untouched.
            return Ok(());
        }

        let policy = if self.interactive {
            Policy::Interactive
        } else {
            Policy::Script
        };
        let resolver = self.reef_resolver();
        let mut lock = self.reef_lock.clone();
        let mut notice: Option<LockNotice> = None;
        let outcome = resolver.resolve(&name, &chain, &mut lock, policy, &mut |n| {
            notice = Some(n.clone());
        });
        let resolution = match outcome {
            Ok(r) => r,
            Err(e) => return Err(reef_error_to_val(e, &name, &chain).with_span(span)),
        };

        argv[0] = resolution.path.clone().into_os_string();
        self.reef_lock = lock;
        if let Some(n) = notice {
            self.persist_reef_lock();
            self.emit_lock_notice(&n);
        }

        // Synthesize the child's PATH so legacy children see a coherent world
        // (REEF §4): the reef view dir first, then the ambient PATH tail unless
        // a scope requested hermetic. Never mutates the session env.
        let path_var = self.reef_synth_path(&resolution, &chain, env)?;
        match env.iter_mut().find(|(k, _)| k == "PATH") {
            Some(pair) => pair.1 = path_var,
            None => env.push((OsString::from("PATH"), path_var)),
        }
        Ok(())
    }

    /// Build (or reuse) a content-addressed view dir binding every locked tool,
    /// and return the synthesized `PATH` value (REEF §4). The system tail is the
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
        for (tool, entry) in &self.reef_lock.tools {
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

    /// Emit the one-line auto-lock notice to the statement sink (REEF §2).
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
        if self.sink.is_some() {
            let v = Value::Str(msg.to_string());
            self.emit(&v);
        } else {
            eprintln!("{msg}");
        }
    }
}

/// Convert a [`shoal_reef::ReefError`] into an `ErrorVal`, preserving the stable
/// code and hint. Enriches `reef_not_found` on a constrained tool with the
/// did-you-mean phrasing from REEF §6.
fn reef_error_to_val(e: shoal_reef::ReefError, name: &str, chain: &ScopeChain) -> ErrorVal {
    use shoal_reef::ReefCode;
    let (code, msg) = if e.code == ReefCode::NotFound {
        let constraint = chain
            .nearest_for(name)
            .map(|s| s.manifest.tools[name].constraint.to_string());
        match constraint {
            Some(c) => (
                e.code_str(),
                format!("`{name}` is constrained ({c}) but not installed — reef fetch {name}"),
            ),
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
