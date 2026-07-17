//! The single authoritative child-evaluator constructor (HR-B1, deep-audit
//! finding B1–B4/H1).
//!
//! Every route that runs Shoal code in a fresh `Evaluator` derived from a
//! running session — `spawn { }` (`script.rs`), a `.shl` script
//! (`script.rs::run_script_file`), `parallel(...)` (`host.rs`), and
//! `on(channel, handler)` (`channels.rs`) — MUST build that child through
//! [`Evaluator::child_context`] + [`ChildContext::build`]. Before this seam
//! existed each site hand-copied a *subset* of fields, and the copies drifted:
//! the audit found the active leash policy/principal, reef scope/resolver/
//! overrides, and (for some routes) config and the event bus silently dropped,
//! so a command a policy forbids foreground could run unconfined inside a
//! `spawn`/`parallel`/handler/script.
//!
//! [`ChildContext`] captures the explicitly enumerated inheritable session
//! context in one place ([`Evaluator::child_context`]) and re-applies it in one place
//! ([`ChildContext::build`], which destructures the struct so the compiler
//! forces every *captured* field to be handled). Adding an `Evaluator` field
//! still requires a deliberate inheritance audit: Rust cannot infer that the
//! new field belongs in this separate struct. Once captured, forgetting to
//! re-apply it is a compile error rather than silent route-by-route drift.
//!
//! Fields that are deliberately NOT inherited (a child gets fresh state) are
//! documented inline in [`ChildContext::build`], each with the rule for why.

use super::*;

/// Whether a child evaluator inherits the parent's lexical environment handle
/// or starts from a fresh session root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildScope {
    /// Share the parent's [`Env`] handle: `spawn`/`parallel`/`on` bodies are
    /// closures/blocks that must see the caller's bindings. The handle is
    /// interior-mutable and parent-linked, so the child observes the same scope.
    Inherit,
    /// Start from a fresh session root (`Env::root()`): a `.shl` script is a
    /// separate program whose `let`s must not leak back into the caller session.
    Fresh,
}

/// A snapshot of the explicitly audited inheritable session capabilities,
/// captured from a parent [`Evaluator`] via [`Evaluator::child_context`] and
/// consumed exactly once by [`ChildContext::build`] to construct a child. All
/// fields are cheap to clone (`Arc` handles or small owned data) and `Send`, so
/// the context can be moved into a worker thread and built there.
pub(crate) struct ChildContext {
    session: session_ctx::SessionCtx,
    cwd: PathBuf,
    env: Env,
    process_env: Vec<(OsString, OsString)>,
    adapters: AdapterCatalog,
    bus: Arc<channels::EventBus>,
    fs: Arc<dyn Fs>,
    exec: Arc<dyn Exec>,
    clock: Arc<dyn Clock>,
    opener: Arc<dyn Opener>,
    secrets: Arc<dyn SecretPort>,
    config: Arc<dyn ConfigPort>,
    reef_chain: Option<(PathBuf, shoal_reef::ScopeChain)>,
    reef_resolver: Option<Arc<shoal_reef::Resolver>>,
    reef_lock: shoal_reef::Lockfile,
    reef_lock_path: Option<PathBuf>,
    reef_user_manifest: Option<PathBuf>,
    reef_overrides: Vec<shoal_reef::ScopeEntry>,
}

impl Evaluator {
    /// Capture the audited inheritable session context for a child evaluator (the
    /// ONLY supported way to seed a child — see [`ChildContext`]). Cheap: `Arc`
    /// clones plus small owned data. The returned context is `Send`, so a route
    /// may move it into a worker thread and call [`ChildContext::build`] there.
    pub(crate) fn child_context(&self) -> ChildContext {
        ChildContext {
            session: self.session.for_child(),
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            process_env: self.process_env.clone(),
            adapters: self.adapters.clone(),
            bus: self.bus.clone(),
            fs: self.fs.clone(),
            exec: self.exec.clone(),
            clock: self.clock.clone(),
            opener: self.opener.clone(),
            secrets: self.secrets.clone(),
            config: self.config.clone(),
            reef_chain: self.reef_chain.clone(),
            reef_resolver: self.reef_resolver.clone(),
            reef_lock: self.reef_lock.clone(),
            reef_lock_path: self.reef_lock_path.clone(),
            reef_user_manifest: self.reef_user_manifest.clone(),
            reef_overrides: self.reef_overrides.clone(),
        }
    }
}

impl ChildContext {
    /// Build the child evaluator. `scope` selects lexical-environment
    /// inheritance; `cancel` is the cancellation token the child consults —
    /// callers pass either the parent's token (`Evaluator::cancellation_token`,
    /// so parent cancellation reaches a synchronous script or a `parallel`
    /// batch) or a FRESH token wired to a spawned task's cancel hook
    /// (`spawn`/`on`, so cancelling the task cancels its child).
    ///
    /// Every OTHER captured capability is applied here by construction: the
    /// method destructures the whole [`ChildContext`], so the compiler rejects
    /// any captured field that is not re-applied to the child.
    pub(crate) fn build(self, scope: ChildScope, cancel: CancelToken) -> Evaluator {
        let ChildContext {
            session,
            cwd,
            env,
            process_env,
            adapters,
            bus,
            fs,
            exec,
            clock,
            opener,
            secrets,
            config,
            reef_chain,
            reef_resolver,
            reef_lock,
            reef_lock_path,
            reef_user_manifest,
            reef_overrides,
        } = self;

        let mut child = Evaluator::new(cwd);
        // Session identity, policy, echo mode, and the deliberate clearing of
        // terminal/root-only handles arrive as one required typed value.
        child.session = session;

        // --- Inherited by construction -------------------------------------
        // Lexical env: closure/spawn/parallel/on bodies inherit the caller's
        // bindings; a `.shl` script keeps `Evaluator::new`'s fresh root.
        if let ChildScope::Inherit = scope {
            child.env = env;
        }
        child.cancel = cancel;
        child.process_env = process_env;
        child.adapters = adapters;
        child.bus = bus;
        // Effect ports (Fs/Exec/Clock/Opener/Secret/Config): a child must see
        // the same fakes/host adapters, or in-process effects diverge.
        child.fs = fs;
        child.exec = exec;
        child.clock = clock;
        child.opener = opener;
        child.secrets = secrets;
        child.config = config;
        // Reef scope/resolver/lock/overrides: constrained tool resolution must
        // resolve identically inside a child, or a pinned tool diverges.
        child.reef_chain = reef_chain;
        child.reef_resolver = reef_resolver;
        child.reef_lock = reef_lock;
        child.reef_lock_path = reef_lock_path;
        child.reef_user_manifest = reef_user_manifest;
        child.reef_overrides = reef_overrides;

        // --- Deliberately NOT inherited (fresh state per child) ------------
        // journal handle:  the current `Journal` is an owned, single-connection
        //                  handle and is not part of ChildContext. The parent
        //                  journals the outer `spawn`/`parallel`/`on`/`run`
        //                  statement; children do not create nested entries.
        //                  session_id/principal still propagate as attribution
        //                  context. A future nested-journal design needs an
        //                  explicit synchronized handle/factory and lifecycle.
        // interactive:     false — a child never owns the real terminal.
        // sink:            no competing mutable renderer; a child returns its
        //                  value through its task/return channel, not a sink.
        // it / plans / modules / jobs / dir_stack / oldpwd / current_entry /
        // source / pending_exit / pending_stop / external_jobs / call_depth /
        // in_fn_body:      per-evaluator session state; a child gets its own.
        // jump_store:      None — a child never writes interactive cd frecency.
        // echo_mode:       fresh default (`All`); only `run_script_file` runs a
        //                  full program, and it keeps the standalone-script
        //                  echo default rather than adopting a host's Quiet mode
        //                  (inheriting it would change script rendering).
        child
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shoal_reef::provider::SystemProvider;
    use shoal_reef::{LockEntry, ManifestKind, ReefManifest, Resolver, ScopeChain, ScopeEntry};

    #[test]
    fn child_context_copies_reef_state_and_journal_attribution_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        let mut parent = Evaluator::new(dir.path().to_path_buf());
        parent.set_journal(
            shoal_journal::Journal::in_memory().unwrap(),
            "session-a",
            "agent:auditor",
        );
        parent.set_leash_policy(LeashPolicy::permissive("agent:auditor"), "agent:auditor");
        parent.set_echo_mode(EchoMode::Quiet);
        parent.set_interactive(true);
        parent.set_statement_sink(Box::new(|_| {}));
        parent.reef_lock.insert(LockEntry {
            name: "fixture".into(),
            version: "1.0.0".into(),
            provider: "system".into(),
            path: dir.path().join("fixture"),
            blake3: "deadbeef".into(),
            resolved_at: "2026-07-16T00:00:00Z".into(),
        });
        parent.reef_lock_path = Some(dir.path().join("reef.lock"));
        parent.reef_user_manifest = Some(dir.path().join("shoal.toml"));
        let scope = ScopeEntry {
            kind: ManifestKind::Reef,
            source: dir.path().join(".reef.toml"),
            manifest: ReefManifest::parse_reef("[tools]\nfixture = \"*\"\n").unwrap(),
            mtime: None,
        };
        parent.reef_chain = Some((
            dir.path().to_path_buf(),
            ScopeChain {
                cwd: dir.path().to_path_buf(),
                scopes: vec![scope.clone()],
            },
        ));
        parent.reef_overrides.push(scope.clone());
        let resolver = Arc::new(Resolver::new(vec![Box::new(SystemProvider::new(
            vec![dir.path().join("bin")],
            vec![],
        ))]));
        parent.reef_resolver = Some(resolver.clone());

        let child = parent
            .child_context()
            .build(ChildScope::Fresh, CancelToken::new());

        assert_eq!(child.session.session_id, "session-a");
        assert_eq!(child.session.principal, "agent:auditor");
        assert!(child.session.leash.is_some());
        assert_eq!(child.session.echo_mode, EchoMode::Quiet);
        assert!(
            !child.session.interactive,
            "a child never owns the terminal"
        );
        assert!(child.session.sink.is_none(), "a child has no host renderer");
        assert!(
            !child.has_journal(),
            "the outer parent statement owns journaling; nested entries are not implied"
        );
        assert_eq!(child.reef_lock, parent.reef_lock);
        assert_eq!(child.reef_lock_path, parent.reef_lock_path);
        assert_eq!(child.reef_user_manifest, parent.reef_user_manifest);
        let (_, child_chain) = child.reef_chain.as_ref().unwrap();
        assert_eq!(child_chain.scopes.len(), 1);
        assert_eq!(child_chain.scopes[0].source, scope.source);
        assert_eq!(child.reef_overrides.len(), 1);
        assert_eq!(child.reef_overrides[0].source, scope.source);
        assert!(Arc::ptr_eq(
            child.reef_resolver.as_ref().unwrap(),
            &resolver
        ));
    }
}
