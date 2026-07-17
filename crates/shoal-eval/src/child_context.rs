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

/// Every production reason a running evaluator may create a child. Naming the
/// route here makes inheritance policy exhaustive instead of letting each call
/// site choose individual fields or even a raw "fresh/inherit" boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildKind {
    Spawn,
    Script,
    Parallel,
    OnHandler,
    StreamPump,
}

/// A snapshot of the explicitly audited inheritable session capabilities,
/// captured from a parent [`Evaluator`] via [`Evaluator::child_context`] and
/// consumed exactly once by [`ChildContext::build`] to construct a child. All
/// fields are cheap to clone (`Arc` handles or small owned data) and `Send`, so
/// the context can be moved into a worker thread and built there.
pub(crate) struct ChildContext {
    host: Arc<host_services::HostServices>,
    session: session_ctx::SessionCtx,
    exec: exec_state::ChildExecSeed,
}

impl Evaluator {
    /// Capture the audited inheritable session context for a child evaluator (the
    /// ONLY supported way to seed a child — see [`ChildContext`]). Cheap: `Arc`
    /// clones plus small owned data. The returned context is `Send`, so a route
    /// may move it into a worker thread and call [`ChildContext::build`] there.
    pub(crate) fn child_context(&self) -> ChildContext {
        ChildContext {
            host: self.host.clone(),
            session: self.session.for_child(),
            exec: self.exec.child_seed(),
        }
    }
}

impl ChildContext {
    /// Build the child evaluator. `kind` exhaustively selects the route and its
    /// lexical-environment rule; `cancel` is the token the child consults —
    /// callers pass either the parent's token (`Evaluator::cancellation_token`,
    /// so parent cancellation reaches a synchronous script or a `parallel`
    /// batch) or a FRESH token wired to a spawned task's cancel hook
    /// (`spawn`/`on`, so cancelling the task cancels its child).
    ///
    /// Every OTHER captured capability is applied here by construction: the
    /// method destructures the whole [`ChildContext`], so the compiler rejects
    /// any captured field that is not re-applied to the child.
    pub(crate) fn build(self, kind: ChildKind, cancel: CancelToken) -> Evaluator {
        let ChildContext {
            host,
            session,
            exec,
        } = self;

        let child = Evaluator {
            host,
            session,
            exec: exec_state::ExecState::child(exec, kind, cancel),
        };

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
        // it / plans / modules / jobs / current_entry / source / pending_exit /
        // pending_stop / external_jobs / call_depth /
        // in_fn_body:      per-evaluator session state; a child gets its own.
        // oldpwd / dir_stack: inherited snapshots, because they are part of the
        //                  caller's dynamic directory context.
        // jump_store:      None — a child never writes interactive cd frecency.
        // echo_mode:       inherited as part of SessionCtx; a Quiet parent
        //                  cannot accidentally create a noisy child.
        child
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shoal_reef::provider::SystemProvider;
    use shoal_reef::{LockEntry, ManifestKind, ReefManifest, Resolver, ScopeChain, ScopeEntry};

    #[test]
    fn production_child_sites_cannot_bypass_child_context() {
        for (name, source) in [
            ("script", include_str!("script.rs")),
            ("host", include_str!("host.rs")),
            ("channels", include_str!("channels/eval.rs")),
        ] {
            let production = source.split("#[cfg(test)]").next().unwrap_or(source);
            assert!(
                !production.contains("Evaluator::new("),
                "{name} contains a manual child constructor"
            );
            assert!(
                production.contains("child_context()"),
                "{name} must use the sole typed child-construction path"
            );
        }
    }

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
        parent
            .env_mut()
            .declare("parent_only", Value::Int(1), false);
        parent.exec.shell.oldpwd = Some(dir.path().join("previous"));
        parent.exec.shell.dir_stack.push(dir.path().join("stacked"));
        parent.exec.reef.lock.insert(LockEntry {
            name: "fixture".into(),
            version: "1.0.0".into(),
            provider: "system".into(),
            path: dir.path().join("fixture"),
            blake3: "deadbeef".into(),
            resolved_at: "2026-07-16T00:00:00Z".into(),
        });
        parent.exec.reef.lock_path = Some(dir.path().join("reef.lock"));
        parent.set_reef_user_manifest(dir.path().join("shoal.toml"));
        let scope = ScopeEntry {
            kind: ManifestKind::Reef,
            source: dir.path().join(".reef.toml"),
            manifest: ReefManifest::parse_reef("[tools]\nfixture = \"*\"\n").unwrap(),
            mtime: None,
        };
        parent.exec.reef.chain = Some((
            dir.path().to_path_buf(),
            ScopeChain {
                cwd: dir.path().to_path_buf(),
                scopes: vec![scope.clone()],
            },
        ));
        parent.exec.reef.overrides.push(scope.clone());
        let resolver = Arc::new(Resolver::new(vec![Box::new(SystemProvider::new(
            vec![dir.path().join("bin")],
            vec![],
        ))]));
        parent.set_reef_resolver(resolver.clone());

        let child = parent
            .child_context()
            .build(ChildKind::Script, CancelToken::new());

        assert!(
            Arc::ptr_eq(&child.host, &parent.host),
            "a child inherits one immutable HostServices snapshot"
        );
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
            child.env().get("parent_only").is_none(),
            "a fresh-scope script child gets a new lexical root"
        );
        assert_eq!(child.exec.shell.oldpwd, parent.exec.shell.oldpwd);
        assert_eq!(child.exec.shell.dir_stack, parent.exec.shell.dir_stack);
        assert!(
            !child.has_journal(),
            "the outer parent statement owns journaling; nested entries are not implied"
        );
        assert_eq!(child.exec.reef.lock, parent.exec.reef.lock);
        assert_eq!(child.exec.reef.lock_path, parent.exec.reef.lock_path);
        assert_eq!(
            child.host.reef_user_manifest,
            parent.host.reef_user_manifest
        );
        let (_, child_chain) = child.exec.reef.chain.as_ref().unwrap();
        assert_eq!(child_chain.scopes.len(), 1);
        assert_eq!(child_chain.scopes[0].source, scope.source);
        assert_eq!(child.exec.reef.overrides.len(), 1);
        assert_eq!(child.exec.reef.overrides[0].source, scope.source);
        assert!(Arc::ptr_eq(
            child.host.reef_resolver.get().unwrap(),
            &resolver
        ));
    }
}
