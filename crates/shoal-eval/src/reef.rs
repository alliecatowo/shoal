//! reef integration for the evaluator (docs/REEF.md §1–§6).
//!
//! The whole path is gated so that a repo with **no** `.reef.toml` (and no user
//! `[reef]` config) behaves EXACTLY as before: [`Evaluator::reef_apply`] fast-
//! bails to today's PATH/`which` behavior whenever the cached scope chain has no
//! manifest entry for the command head. Only a *constrained* head (a tool a
//! manifest in scope actually mentions) engages the resolver, the lock, the
//! interactive/script policy split, and child-PATH synthesis.

use super::*;

use shoal_reef::{
    Binding, LockNotice, ManifestKind, Policy, ProviderCtx, ResolutionReport, Resolver, ScopeChain,
    ViewConfig, default_view_root, synth_path,
};

impl Evaluator {
    // --- chain cache -------------------------------------------------------

    /// Ensure the cached scope chain matches the current cwd. Rebuilds only when
    /// the cwd changed since the last discovery (so `cd` / `with cwd:` re-scope
    /// the next resolution and nothing else does). Reloads the lock next to the
    /// nearest manifest at the same time.
    fn ensure_reef_chain(&mut self) {
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

    /// A clone of the current scope chain (cheap: manifests are small maps). The
    /// clone frees `self` for the resolver/lock mutations that follow.
    fn reef_chain_snapshot(&mut self) -> ScopeChain {
        self.ensure_reef_chain();
        self.reef_chain.as_ref().expect("just ensured").1.clone()
    }

    /// The lazily-built provider stack (REEF §3). Only ever called once a
    /// manifest is in scope, so the no-manifest hot path never constructs it.
    fn reef_resolver(&mut self) -> Arc<Resolver> {
        if self.reef_resolver.is_none() {
            self.reef_resolver = Some(Arc::new(Resolver::with_defaults()));
        }
        self.reef_resolver.as_ref().expect("just set").clone()
    }

    /// True when at least one manifest constrains something in the current
    /// scope. The single gate that keeps the no-manifest world untouched.
    fn reef_manifest_in_scope(&mut self) -> bool {
        self.ensure_reef_chain();
        !self.reef_chain.as_ref().expect("ensured").1.scopes.is_empty()
    }

    /// Persist the in-memory lock next to its manifest, best-effort. A failure
    /// to write never fails a spawn — the lock is an optimization, not a gate.
    fn persist_reef_lock(&self) {
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

    // --- `which` (REEF §6) -------------------------------------------------

    /// `which <tool>` → a resolution report record; `which <tool> --all` → a
    /// table of every candidate. With no manifest in scope, `which git` still
    /// finds the ambient PATH entry (a minimal report), never a regression.
    pub(crate) fn builtin_which(&mut self, call: &CmdCall) -> VResult<Value> {
        let mut names = Vec::new();
        let mut all = false;
        for a in &call.args {
            match a {
                CmdArg::FlagLong { name, .. } if name == "all" => all = true,
                CmdArg::FlagShort { chars, .. } if chars.contains('a') => all = true,
                CmdArg::FlagLong { .. } | CmdArg::FlagShort { .. } | CmdArg::DashDash { .. } => {}
                _ => {
                    for v in self.expand_arg(a)? {
                        names.push(reef_value_word(&v)?);
                    }
                }
            }
        }
        if names.len() != 1 {
            return Err(ErrorVal::arg_error("which requires exactly one command"));
        }
        let name = names.into_iter().next().expect("one name");

        if all {
            return self.which_all(&name);
        }

        let chain = self.reef_chain_snapshot();
        let resolver = self.reef_resolver();
        let mut lock = self.reef_lock.clone();
        match resolver.resolve(&name, &chain, &mut lock, Policy::Interactive, &mut |_| {}) {
            Ok(res) => {
                // Only keep a fresh lock when a manifest actually constrained it.
                if res.constrained {
                    self.reef_lock = lock;
                    if res.locked_now {
                        self.persist_reef_lock();
                    }
                }
                Ok(report_to_record(&res.report))
            }
            Err(_) => {
                // Fall back to the ambient PATH lookup so `which` never regresses.
                let path_env = self
                    .process_env
                    .iter()
                    .find(|(k, _)| k == "PATH")
                    .map(|(_, v)| v.as_os_str());
                match shoal_exec::which(OsStr::new(&name), path_env) {
                    Some(p) => Ok(minimal_which_record(&name, &p)),
                    None => Ok(Value::Null),
                }
            }
        }
    }

    /// `which <tool> --all`: every candidate every provider offers, as a table.
    fn which_all(&mut self, name: &str) -> VResult<Value> {
        let resolver = self.reef_resolver();
        let ctx = ProviderCtx::new(self.cwd.clone());
        let mut rows = Vec::new();
        for provider in resolver.providers() {
            for cand in provider.discover(name, &ctx) {
                let mut r = Record::new();
                r.insert("tool".into(), Value::Str(cand.tool.clone()));
                r.insert("version".into(), Value::Str(cand.version.to_string()));
                r.insert("path".into(), Value::Path(cand.path.clone()));
                r.insert("provider".into(), Value::Str(provider.name().to_string()));
                r.insert(
                    "scope".into(),
                    Value::Str(if cand.ambient { "ambient" } else { "system" }.into()),
                );
                rows.push(r);
            }
        }
        Ok(Value::Table(rows))
    }

    // --- `reef` builtins (REEF §6) -----------------------------------------

    /// The `reef` builtin family: bare `reef` (binding table), `reef add
    /// <tool>@<ver>`, `reef lock [--refresh]`, `reef fetch <tool>`.
    pub(crate) fn builtin_reef(&mut self, call: &CmdCall) -> VResult<Value> {
        let mut words = Vec::new();
        let mut flags = Vec::new();
        for a in &call.args {
            match a {
                CmdArg::FlagLong { name, .. } => flags.push(name.clone()),
                CmdArg::FlagShort { chars, .. } => {
                    flags.extend(chars.chars().map(|c| c.to_string()))
                }
                CmdArg::DashDash { .. } => {}
                _ => {
                    for v in self.expand_arg(a)? {
                        words.push(reef_value_word(&v)?);
                    }
                }
            }
        }
        match words.first().map(String::as_str) {
            None => self.reef_binding_table(),
            Some("add") => self.reef_add(words.get(1).map(String::as_str)),
            Some("lock") => self.reef_lock_cmd(flags.iter().any(|f| f == "refresh")),
            Some("fetch") => self.reef_fetch(words.get(1).map(String::as_str)),
            Some(other) => Err(ErrorVal::arg_error(format!(
                "reef: unknown subcommand `{other}` (expected add, lock, or fetch)"
            ))),
        }
    }

    /// Bare `reef`: the current binding table for every constrained tool.
    fn reef_binding_table(&mut self) -> VResult<Value> {
        let chain = self.reef_chain_snapshot();
        let mut names: Vec<String> = Vec::new();
        for scope in &chain.scopes {
            for tool in scope.manifest.tools.keys() {
                if !names.contains(tool) {
                    names.push(tool.clone());
                }
            }
        }
        names.sort();
        let resolver = self.reef_resolver();
        let mut lock = self.reef_lock.clone();
        let mut rows = Vec::new();
        for name in names {
            let mut r = Record::new();
            r.insert("name".into(), Value::Str(name.clone()));
            match resolver.resolve(&name, &chain, &mut lock, Policy::Interactive, &mut |_| {}) {
                Ok(res) => {
                    r.insert("constraint".into(), Value::Str(res.report.constraint.clone()));
                    r.insert("version".into(), Value::Str(res.version.to_string()));
                    r.insert("hash8".into(), Value::Str(short_hash(&res.hash)));
                    r.insert("provider".into(), Value::Str(res.provider.clone()));
                    r.insert("scope".into(), Value::Str(res.report.scope.clone()));
                }
                Err(e) => {
                    let c = chain
                        .nearest_for(&name)
                        .map(|s| s.manifest.tools[&name].constraint.to_string())
                        .unwrap_or_default();
                    r.insert("constraint".into(), Value::Str(c));
                    r.insert("version".into(), Value::Null);
                    r.insert("hash8".into(), Value::Null);
                    r.insert("provider".into(), Value::Null);
                    r.insert("scope".into(), Value::Str(format!("unresolved: {}", e.code_str())));
                }
            }
            rows.push(r);
        }
        self.reef_lock = lock;
        Ok(Value::Table(rows))
    }

    /// `reef add <tool>@<ver>`: write the nearest `.reef.toml` (creating one in
    /// cwd if none) and lock the tool.
    fn reef_add(&mut self, spec: Option<&str>) -> VResult<Value> {
        let spec = spec.ok_or_else(|| ErrorVal::arg_error("reef add expects <tool>@<version>"))?;
        let (tool, ver) = match spec.split_once('@') {
            Some((t, v)) if !t.is_empty() && !v.is_empty() => (t.to_string(), v.to_string()),
            _ => {
                return Err(ErrorVal::arg_error(format!(
                    "reef add: expected <tool>@<version>, found `{spec}`"
                )));
            }
        };
        // The nearest native manifest, or a new one in cwd.
        let manifest_path = {
            let chain = self.reef_chain_snapshot();
            chain
                .scopes
                .iter()
                .find(|s| s.kind == ManifestKind::Reef)
                .map(|s| s.source.clone())
                .unwrap_or_else(|| self.cwd.join(".reef.toml"))
        };
        let mut doc = match std::fs::read_to_string(&manifest_path) {
            Ok(text) => text
                .parse::<toml::Table>()
                .map_err(|e| ErrorVal::new("reef_provider", format!("parsing manifest: {e}")))?,
            Err(_) => toml::Table::new(),
        };
        let tools = doc
            .entry("tools".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let toml::Value::Table(tools) = tools else {
            return Err(ErrorVal::new(
                "reef_provider",
                "manifest [tools] is not a table",
            ));
        };
        tools.insert(tool.clone(), toml::Value::String(ver.clone()));
        std::fs::write(&manifest_path, doc.to_string())
            .map_err(|e| ErrorVal::new("reef_provider", format!("writing manifest: {e}")))?;

        // Re-discover so the fresh constraint is in scope, then lock it.
        self.reef_chain = None;
        let chain = self.reef_chain_snapshot();
        let resolver = self.reef_resolver();
        let mut lock = self.reef_lock.clone();
        let mut r = Record::new();
        r.insert("added".into(), Value::Str(format!("{tool}@{ver}")));
        r.insert("manifest".into(), Value::Path(manifest_path.clone()));
        match resolver.refresh_lock(&tool, &chain, &mut lock, &mut |_| {}) {
            Ok(res) => {
                self.reef_lock = lock;
                self.persist_reef_lock();
                r.insert("version".into(), Value::Str(res.version.to_string()));
                r.insert("path".into(), Value::Path(res.path.clone()));
                r.insert("locked".into(), Value::Bool(true));
            }
            Err(e) => {
                // The manifest edit stands; the lock could not be written.
                r.insert("locked".into(), Value::Bool(false));
                r.insert("note".into(), Value::Str(e.to_string()));
            }
        }
        Ok(Value::Record(r))
    }

    /// `reef lock [--refresh]`: resolve and lock every constrained tool.
    fn reef_lock_cmd(&mut self, refresh: bool) -> VResult<Value> {
        let chain = self.reef_chain_snapshot();
        if chain.scopes.is_empty() {
            return Err(ErrorVal::new(
                "reef_not_found",
                "reef lock: no manifest in scope",
            ));
        }
        let mut names: Vec<String> = Vec::new();
        for scope in &chain.scopes {
            for tool in scope.manifest.tools.keys() {
                if !names.contains(tool) {
                    names.push(tool.clone());
                }
            }
        }
        names.sort();
        let resolver = self.reef_resolver();
        let mut lock = self.reef_lock.clone();
        let mut rows = Vec::new();
        for name in names {
            let mut r = Record::new();
            r.insert("name".into(), Value::Str(name.clone()));
            let res = if refresh {
                resolver.refresh_lock(&name, &chain, &mut lock, &mut |_| {})
            } else {
                resolver.resolve(&name, &chain, &mut lock, Policy::Interactive, &mut |_| {})
            };
            match res {
                Ok(res) => {
                    r.insert("version".into(), Value::Str(res.version.to_string()));
                    r.insert("hash8".into(), Value::Str(short_hash(&res.hash)));
                    r.insert("locked".into(), Value::Bool(true));
                }
                Err(e) => {
                    r.insert("locked".into(), Value::Bool(false));
                    r.insert("error".into(), Value::Str(e.code_str().to_string()));
                }
            }
            rows.push(r);
        }
        self.reef_lock = lock;
        self.persist_reef_lock();
        Ok(Value::Table(rows))
    }

    /// `reef fetch <tool>`: delegate to the tool's provider(s); may no-op when
    /// no provider can install.
    fn reef_fetch(&mut self, tool: Option<&str>) -> VResult<Value> {
        let tool = tool.ok_or_else(|| ErrorVal::arg_error("reef fetch expects a tool name"))?;
        let chain = self.reef_chain_snapshot();
        let constraint = chain
            .nearest_for(tool)
            .map(|s| s.manifest.tools[tool].constraint.clone())
            .unwrap_or(shoal_reef::Constraint::Any);
        let resolver = self.reef_resolver();
        let ctx = ProviderCtx::new(self.cwd.clone());
        let mut r = Record::new();
        r.insert("tool".into(), Value::Str(tool.to_string()));
        for provider in resolver.providers() {
            match provider.fetch(tool, &constraint, &ctx) {
                Some(Ok(cand)) => {
                    r.insert("fetched".into(), Value::Bool(true));
                    r.insert("provider".into(), Value::Str(provider.name().to_string()));
                    r.insert("version".into(), Value::Str(cand.version.to_string()));
                    r.insert("path".into(), Value::Path(cand.path.clone()));
                    return Ok(Value::Record(r));
                }
                Some(Err(e)) => {
                    r.insert("fetched".into(), Value::Bool(false));
                    r.insert("error".into(), Value::Str(e.to_string()));
                    return Ok(Value::Record(r));
                }
                None => continue,
            }
        }
        r.insert("fetched".into(), Value::Bool(false));
        r.insert(
            "note".into(),
            Value::Str("no provider can fetch this tool".into()),
        );
        Ok(Value::Record(r))
    }

    // --- runners (REEF §5) -------------------------------------------------

    /// When a manifest is in scope, resolve the runner for `path` through reef
    /// (extension → tool, shebang fallback) and return the argv template
    /// (`[tool, ...args_template]`) whose tool the spawn will itself reef-
    /// resolve. `None` ⇒ no manifest in scope or `self`-runner (`.shl`): the
    /// caller keeps today's behavior. A pure lookup — no spawning here.
    pub(crate) fn reef_runner_argv(&mut self, path: &Path) -> Option<Vec<OsString>> {
        if !self.reef_manifest_in_scope() {
            return None;
        }
        let chain = self.reef_chain_snapshot();
        let table = chain.runner_table();
        let inv = shoal_reef::resolve_runner(path, &table)?;
        if inv.tool == "self" {
            return None;
        }
        let mut argv: Vec<OsString> = vec![OsString::from(&inv.tool)];
        argv.extend(inv.args_template.iter().map(OsString::from));
        Some(argv)
    }
}

// --- free helpers ----------------------------------------------------------

/// Map a resolved [`ResolutionReport`] to the record `which` renders (REEF §6).
fn report_to_record(report: &ResolutionReport) -> Value {
    let mut r = Record::new();
    r.insert("name".into(), Value::Str(report.name.clone()));
    r.insert("scope".into(), Value::Str(report.scope.clone()));
    r.insert("constraint".into(), Value::Str(report.constraint.clone()));
    r.insert("version".into(), Value::Str(report.version.clone()));
    r.insert("path".into(), Value::Path(report.path.clone()));
    r.insert("hash8".into(), Value::Str(short_hash(&report.hash)));
    r.insert("provider".into(), Value::Str(report.provider.clone()));
    let chain = report
        .chain
        .iter()
        .map(|d| {
            let mut row = Record::new();
            row.insert("scope".into(), Value::Str(d.scope.clone()));
            row.insert("source".into(), Value::Path(d.source.clone()));
            row.insert(
                "constraint".into(),
                d.constraint
                    .clone()
                    .map(Value::Str)
                    .unwrap_or(Value::Null),
            );
            row.insert("outcome".into(), Value::Str(d.outcome.clone()));
            row
        })
        .collect();
    r.insert("chain".into(), Value::Table(chain));
    Value::Record(r)
}

/// The minimal `which` record for an ambient PATH hit (no manifest in scope).
fn minimal_which_record(name: &str, path: &Path) -> Value {
    let mut r = Record::new();
    r.insert("name".into(), Value::Str(name.to_string()));
    r.insert("scope".into(), Value::Str("ambient".into()));
    r.insert("constraint".into(), Value::Str("*".into()));
    r.insert("version".into(), Value::Str("unknown".into()));
    r.insert("path".into(), Value::Path(path.to_path_buf()));
    r.insert("hash8".into(), Value::Null);
    r.insert("provider".into(), Value::Str("ambient".into()));
    r.insert("chain".into(), Value::Table(Vec::new()));
    Value::Record(r)
}

/// First 8 hex chars of a blake3 hash (the `hash8` column). Empty stays empty.
fn short_hash(hash: &str) -> String {
    hash.chars().take(8).collect()
}

/// Coerce a command-argument value to a plain name string for reef lookups.
fn reef_value_word(v: &Value) -> VResult<String> {
    match v {
        Value::Str(s) => Ok(s.clone()),
        Value::Path(p) => Ok(p.to_string_lossy().into_owned()),
        other => Err(ErrorVal::type_error(format!(
            "expected a tool name, found {}",
            other.type_name()
        ))),
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
