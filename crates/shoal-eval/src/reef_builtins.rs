//! `which`/`reef` builtins layered on reef resolution (site/content/internals/reef-resolution.md).
//!
//! Split out of [`crate::reef`] (see that module's doc for the split
//! rationale); see [`crate::reef_resolve`] for the scope-chain/resolver
//! mechanics these commands call into.

use super::*;

use shoal_reef::hashcache::HashCache;
use shoal_reef::{
    ManifestKind, Policy, ProviderCtx, ReefCode, ReefError, ResolutionReport, ScopeChain,
};
use shoal_syntax::commands::CommandSource;

impl Evaluator {
    // --- `which` (site/content/internals/reef-resolution.md) -------------------------------------------------

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

        let command = self.resolve_head(&name, false, true);
        if command.source != CommandSource::External {
            return self.command_source_record(&name, &command);
        }

        self.executable_resolution_record(&name)
    }

    fn executable_resolution_record(&mut self, name: &str) -> VResult<Value> {
        let chain = self.reef_chain_snapshot();
        let resolver = self.reef_resolver();
        let mut lock = self.exec.reef.lock.clone();
        match resolver.resolve(name, &chain, &mut lock, Policy::Interactive, &mut |_| {}) {
            Ok(res) => {
                // Only keep a fresh lock when a manifest actually constrained it.
                if res.constrained {
                    self.exec.reef.lock = lock;
                    if res.locked_now {
                        self.persist_reef_lock();
                    }
                }
                Ok(report_to_record(&res.report))
            }
            // A genuine "nothing anywhere provides this" miss falls back to
            // the ambient PATH lookup so `which` never regresses today's
            // behavior for an ordinary, unconstrained command.
            Err(e) if e.code == ReefCode::NotFound => {
                let path_env = self
                    .exec
                    .shell
                    .process_env
                    .iter()
                    .find(|(k, _)| k == "PATH")
                    .map(|(_, v)| v.as_os_str());
                match shoal_exec::which(OsStr::new(name), path_env) {
                    Some(p) => {
                        let hash = self.hash_resolved_bin(p.as_os_str());
                        Ok(minimal_which_record(name, &p, hash.as_deref()))
                    }
                    None => Ok(Value::Null),
                }
            }
            // Conflict/Drift/Unlocked/Provider are real protection states —
            // `which` must surface them, not silently guess an unconstrained
            // ambient binary and report it as if reef had nothing to say
            // (the audit's single most user-misleading finding: `which`
            // actively lied about protection). Mirrors the "unresolved:
            // <code>" idiom `reef_binding_table` already uses below.
            Err(e) => Ok(unresolved_which_record(name, &e, &chain)),
        }
    }

    fn command_source_record(
        &mut self,
        name: &str,
        resolution: &crate::resolution::CommandResolution,
    ) -> VResult<Value> {
        let source = resolution.source;
        let mut record = Record::new();
        record.insert("name".into(), Value::Str(name.to_string()));
        record.insert("source".into(), Value::Str(source.as_str().into()));
        record.insert("reason".into(), Value::Str(source.reason().into()));
        record.insert("scope".into(), Value::Str(source.as_str().into()));
        record.insert("constraint".into(), Value::Str("*".into()));
        record.insert("version".into(), Value::Null);
        record.insert("chain".into(), Value::Table(Vec::new()));

        let mut path = None;
        let mut hash = None;
        if source == CommandSource::Script {
            let candidate = PathBuf::from(name);
            path = Some(if candidate.is_absolute() {
                candidate
            } else {
                self.exec.shell.cwd.join(candidate)
            });
        }
        if source == CommandSource::Adapter {
            let adapter = self
                .host
                .adapters
                .lookup(name)
                .cloned()
                .expect("adapter resolution carries a catalog entry");
            let executable = self.executable_resolution_record(&adapter.bin)?;
            if let Value::Record(executable_record) = &executable {
                path = executable_record.get("path").and_then(|value| match value {
                    Value::Path(path) => Some(path.clone()),
                    _ => None,
                });
                hash = executable_record.get("hash").and_then(|value| match value {
                    Value::Str(hash) => Some(hash.clone()),
                    _ => None,
                });
                if let Some(provider) = executable_record.get("provider") {
                    record.insert("executable_provider".into(), provider.clone());
                }
            }
            record.insert("executable".into(), executable);

            let mut schema = Record::new();
            schema.insert("bin".into(), Value::Str(adapter.bin.clone()));
            schema.insert(
                "class".into(),
                Value::Str(format!("{:?}", adapter.class).to_ascii_lowercase()),
            );
            schema.insert(
                "params".into(),
                Value::List(
                    adapter
                        .top
                        .params
                        .iter()
                        .map(|param| Value::Str(param.name.clone()))
                        .collect(),
                ),
            );
            schema.insert(
                "subcommands".into(),
                Value::List(adapter.subs.keys().cloned().map(Value::Str).collect()),
            );
            record.insert("adapter".into(), Value::Record(schema));
        }
        if let Some(binding) = &resolution.binding {
            record.insert("value_type".into(), Value::Str(binding.type_name().into()));
        }

        record.insert("path".into(), path.map(Value::Path).unwrap_or(Value::Null));
        record.insert(
            "hash8".into(),
            hash.as_ref()
                .map(|value| Value::Str(short_hash(value)))
                .unwrap_or(Value::Null),
        );
        record.insert("hash".into(), hash.map(Value::Str).unwrap_or(Value::Null));
        record.insert("provider".into(), Value::Str(source.as_str().into()));
        Ok(Value::Record(record))
    }

    /// `which <tool> --all`: every candidate every provider offers, as a
    /// table. Unlike singular `which`, this never calls `resolver.resolve()`
    /// — it just enumerates raw candidates per provider (each correctly
    /// labeled `ambient`/`system` from `Candidate::ambient`), so there is no
    /// resolver error to swallow here: no conflict/drift/lock decision is
    /// ever made or hidden, only a plain listing.
    fn which_all(&mut self, name: &str) -> VResult<Value> {
        let resolver = self.reef_resolver();
        let ctx = ProviderCtx::new(self.exec.shell.cwd.clone());
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

    // --- `reef` builtins (site/content/internals/reef-resolution.md) -----------------------------------------

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
            Some("doctor") => self.reef_doctor(),
            Some(other) => Err(ErrorVal::arg_error(format!(
                "reef: unknown subcommand `{other}` (expected add, lock, fetch, or doctor)"
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
        let mut lock = self.exec.reef.lock.clone();
        let mut rows = Vec::new();
        for name in names {
            let mut r = Record::new();
            r.insert("name".into(), Value::Str(name.clone()));
            match resolver.resolve(&name, &chain, &mut lock, Policy::Interactive, &mut |_| {}) {
                Ok(res) => {
                    r.insert(
                        "constraint".into(),
                        Value::Str(res.report.constraint.clone()),
                    );
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
                    r.insert(
                        "scope".into(),
                        Value::Str(format!("unresolved: {}", e.code_str())),
                    );
                }
            }
            rows.push(r);
        }
        self.exec.reef.lock = lock;
        Ok(Value::Table(rows))
    }

    /// `reef add <tool>@<ver>`: write the target `.reef.toml` and lock the tool.
    ///
    /// **Target precedence — the LOCAL manifest always wins (site/content/internals/reef-resolution.md).**
    /// A `.reef.toml` sitting in `cwd` is *this* (sub)project's own manifest, so
    /// `reef add` must edit it — and it is resolved by a direct
    /// [`Fs::is_file`](shoal_value::ports::Fs::is_file) existence probe, NOT by
    /// consulting the parsed scope chain:
    ///
    /// 1. `cwd/.reef.toml` **exists** → that is the target. If it is malformed,
    ///    the read+parse below surfaces the LOCAL parse error and writes
    ///    nothing — never an ancestor.
    /// 2. otherwise → the chain's nearest native `.reef.toml` (a real ancestor
    ///    project), so `reef add` in a bare subdir still writes the project's
    ///    manifest one dir up ("writes nearest manifest", site/content/internals/reef-resolution.md).
    /// 3. otherwise → create a fresh `cwd/.reef.toml`.
    ///
    /// The existence probe (rather than the chain) is load-bearing:
    /// [`ScopeChain::discover`] *silently skips* a malformed `.reef.toml`, so the
    /// chain's nearest **parsed** `Reef` scope can be an ANCESTOR's manifest even
    /// though a broken one sits right here. Selecting the target off the chain
    /// (the old behavior) meant a malformed local manifest under a valid ancestor
    /// caused `reef add` to silently mutate the ANCESTOR — a nasty footgun for a
    /// subproject nested under a project. Probing `cwd` for the file directly
    /// keeps the local manifest authoritative whether it parses or not.
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
        // Local manifest first (via the Fs port so a present-but-malformed one is
        // still seen, unlike the chain which skips it); then the nearest ancestor
        // Reef scope; then a fresh manifest in cwd.
        let local = self.exec.shell.cwd.join(".reef.toml");
        let manifest_path = if self.host.fs.is_file(&local) {
            local
        } else {
            let chain = self.reef_chain_snapshot();
            chain
                .scopes
                .iter()
                .find(|s| s.kind == ManifestKind::Reef)
                .map(|s| s.source.clone())
                .unwrap_or(local)
        };
        let mut doc = match self.host.fs.read_to_string(&manifest_path) {
            Ok(text) => text.parse::<toml::Table>().map_err(|e| {
                ErrorVal::new(
                    "reef_provider",
                    format!("parsing manifest {}: {e}", manifest_path.display()),
                )
            })?,
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
        self.host
            .fs
            .write(&manifest_path, doc.to_string().as_bytes())
            .map_err(|e| ErrorVal::new("reef_provider", format!("writing manifest: {e}")))?;

        // Re-discover so the fresh constraint is in scope, then lock it.
        self.exec.reef.chain = None;
        let chain = self.reef_chain_snapshot();
        let resolver = self.reef_resolver();
        let mut lock = self.exec.reef.lock.clone();
        let mut r = Record::new();
        r.insert("added".into(), Value::Str(format!("{tool}@{ver}")));
        r.insert("manifest".into(), Value::Path(manifest_path.clone()));
        match resolver.refresh_lock(&tool, &chain, &mut lock, &mut |_| {}) {
            Ok(res) => {
                self.exec.reef.lock = lock;
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
        let mut lock = self.exec.reef.lock.clone();
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
        self.exec.reef.lock = lock;
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
        let ctx = ProviderCtx::new(self.exec.shell.cwd.clone());
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

    /// `reef doctor` (site/content/internals/reef-resolution.md): a health report over the current reef
    /// state — one row per finding, never an error (a health check has
    /// nothing to say about a clean or empty scope, so it returns an empty
    /// table rather than `reef lock`'s "no manifest in scope" error):
    ///
    /// - **drift**: for each constrained, LOCKED tool, the on-disk binary is
    ///   re-hashed (reusing `shoal_reef`'s own `HashCache`, the exact logic
    ///   `resolve.rs`'s `resolution_from_lock` uses at spawn time) and
    ///   compared against the lock's recorded hash.
    /// - **orphan**: a `reef.lock` entry whose tool no manifest in the
    ///   current chain mentions anymore (e.g. removed from `.reef.toml` after
    ///   being locked).
    /// - **shadowed_ambient**: a constrained, locked name that ALSO resolves,
    ///   to a DIFFERENT binary, via plain ambient PATH (`ambient_which`,
    ///   shared with the not-found did-you-mean in `reef_resolve.rs`) — the
    ///   ambient copy is invisible to reef but would surprise anyone who
    ///   forgot the project pin exists.
    fn reef_doctor(&mut self) -> VResult<Value> {
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

        let hashes = HashCache::new();
        let mut rows = Vec::new();

        for name in &names {
            let mut r = Record::new();
            r.insert("name".into(), Value::Str(name.clone()));
            r.insert("check".into(), Value::Str("drift".into()));
            match self.exec.reef.lock.get(name) {
                Some(entry) => {
                    let current = hashes.hash_file(&entry.path).ok();
                    let drifted = current.as_deref() != Some(entry.blake3.as_str());
                    r.insert(
                        "status".into(),
                        Value::Str(if drifted { "drift" } else { "ok" }.into()),
                    );
                    r.insert("path".into(), Value::Path(entry.path.clone()));
                    r.insert("locked_hash8".into(), Value::Str(short_hash(&entry.blake3)));
                    r.insert(
                        "current_hash8".into(),
                        match &current {
                            Some(h) => Value::Str(short_hash(h)),
                            None => Value::Null,
                        },
                    );
                }
                None => {
                    r.insert("status".into(), Value::Str("unlocked".into()));
                    r.insert("path".into(), Value::Null);
                    r.insert("locked_hash8".into(), Value::Null);
                    r.insert("current_hash8".into(), Value::Null);
                }
            }
            rows.push(r);

            if let Some(entry) = self.exec.reef.lock.get(name)
                && let Some(ambient) = self.ambient_which(name)
                && ambient != entry.path
            {
                let mut s = Record::new();
                s.insert("name".into(), Value::Str(name.clone()));
                s.insert("check".into(), Value::Str("shadowed_ambient".into()));
                s.insert("status".into(), Value::Str("shadowed".into()));
                s.insert("path".into(), Value::Path(entry.path.clone()));
                s.insert("ambient_path".into(), Value::Path(ambient));
                rows.push(s);
            }
        }

        for (name, entry) in &self.exec.reef.lock.tools {
            if names.contains(name) {
                continue;
            }
            let mut r = Record::new();
            r.insert("name".into(), Value::Str(name.clone()));
            r.insert("check".into(), Value::Str("orphan".into()));
            r.insert("status".into(), Value::Str("orphan".into()));
            r.insert("path".into(), Value::Path(entry.path.clone()));
            rows.push(r);
        }

        Ok(Value::Table(rows))
    }
}

/// Map a resolved [`ResolutionReport`] to the record `which` renders (site/content/internals/reef-resolution.md).
fn report_to_record(report: &ResolutionReport) -> Value {
    let mut r = Record::new();
    r.insert("name".into(), Value::Str(report.name.clone()));
    r.insert("source".into(), Value::Str("external".into()));
    r.insert(
        "reason".into(),
        Value::Str(CommandSource::External.reason().into()),
    );
    r.insert("scope".into(), Value::Str(report.scope.clone()));
    r.insert("constraint".into(), Value::Str(report.constraint.clone()));
    r.insert("version".into(), Value::Str(report.version.clone()));
    r.insert("path".into(), Value::Path(report.path.clone()));
    r.insert("hash8".into(), Value::Str(short_hash(&report.hash)));
    r.insert("hash".into(), Value::Str(report.hash.clone()));
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
                d.constraint.clone().map(Value::Str).unwrap_or(Value::Null),
            );
            row.insert("outcome".into(), Value::Str(d.outcome.clone()));
            row
        })
        .collect();
    r.insert("chain".into(), Value::Table(chain));
    Value::Record(r)
}

/// The `which` record for a resolver error that is NOT a plain "not found"
/// (`reef_conflict`/`reef_drift`/`reef_unlocked`/`reef_provider`): the real
/// protection state, not an ambient guess. Mirrors `reef_binding_table`'s own
/// `"unresolved: {code}"` idiom so `which` and bare `reef` agree on how an
/// unresolved tool renders.
fn unresolved_which_record(name: &str, e: &ReefError, chain: &ScopeChain) -> Value {
    let mut r = Record::new();
    r.insert("name".into(), Value::Str(name.to_string()));
    r.insert("source".into(), Value::Str("external".into()));
    r.insert(
        "reason".into(),
        Value::Str(CommandSource::External.reason().into()),
    );
    let constraint = chain
        .nearest_for(name)
        .map(|s| s.manifest.tools[name].constraint.to_string())
        .unwrap_or_default();
    r.insert(
        "scope".into(),
        Value::Str(format!("unresolved: {}", e.code_str())),
    );
    r.insert("constraint".into(), Value::Str(constraint));
    r.insert("version".into(), Value::Null);
    r.insert("path".into(), Value::Null);
    r.insert("hash8".into(), Value::Null);
    r.insert("hash".into(), Value::Null);
    r.insert("provider".into(), Value::Null);
    r.insert("chain".into(), Value::Table(Vec::new()));
    // The real error message (e.g. reef_drift's old/new hashes, reef_conflict's
    // two sources) — `which` surfacing "the real state" means more than just
    // the bare code.
    r.insert("note".into(), Value::Str(e.msg.clone()));
    if let Some(h) = &e.hint {
        r.insert("hint".into(), Value::Str(h.clone()));
    }
    Value::Record(r)
}

/// The minimal `which` record for an ambient PATH hit (no manifest in scope).
fn minimal_which_record(name: &str, path: &Path, hash: Option<&str>) -> Value {
    let mut r = Record::new();
    r.insert("name".into(), Value::Str(name.to_string()));
    r.insert("source".into(), Value::Str("external".into()));
    r.insert(
        "reason".into(),
        Value::Str(CommandSource::External.reason().into()),
    );
    r.insert("scope".into(), Value::Str("ambient".into()));
    r.insert("constraint".into(), Value::Str("*".into()));
    r.insert("version".into(), Value::Str("unknown".into()));
    r.insert("path".into(), Value::Path(path.to_path_buf()));
    r.insert(
        "hash8".into(),
        hash.map(|value| Value::Str(short_hash(value)))
            .unwrap_or(Value::Null),
    );
    r.insert(
        "hash".into(),
        hash.map(|value| Value::Str(value.to_string()))
            .unwrap_or(Value::Null),
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    fn eval_parsed(ev: &mut Evaluator, src: &str) -> Value {
        let out = ev
            .eval_program(&shoal_syntax::parse(src).unwrap())
            .unwrap_or_else(|e| panic!("{src}: {e}"));
        let Value::Outcome(outcome) = out else {
            panic!("{src}: expected an outcome, got {out:?}")
        };
        outcome
            .parsed
            .as_ref()
            .cloned()
            .unwrap_or_else(|| panic!("{src}: outcome carried no parsed value"))
    }

    /// Run `src` in a fresh `Evaluator` rooted at `cwd`, returning the
    /// resolution/health record `which`/`reef` carry as an outcome's
    /// `.parsed` value (mirrors `crates/shoal-eval/tests/reef_integration.rs`'s
    /// own unwrap pattern).
    fn parsed(cwd: &Path, src: &str) -> Value {
        let mut ev = Evaluator::new(cwd.to_path_buf());
        eval_parsed(&mut ev, src)
    }

    #[test]
    fn which_reports_the_same_winning_source_as_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let mut ev = Evaluator::new(dir.path().into());
        ev.eval_program(&shoal_syntax::parse("fn deploy() { null }").unwrap())
            .unwrap();
        ev.env_mut().declare("answer", Value::Int(42), false);

        for (head, expected) in [
            ("deploy", "session_callable"),
            ("answer", "bound_value"),
            ("ls", "structured_builtin"),
            ("cd", "special_builtin"),
        ] {
            let Value::Record(record) = eval_parsed(&mut ev, &format!("which {head}")) else {
                panic!("which {head}: expected record")
            };
            assert_eq!(
                record.get("source"),
                Some(&Value::Str(expected.into())),
                "which {head} diverged from runtime precedence"
            );
            assert!(matches!(record.get("reason"), Some(Value::Str(reason)) if !reason.is_empty()));
        }
    }

    #[test]
    fn which_adapter_trace_includes_schema_and_executable_resolution() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tool.toml"),
            r#"
[cmd.audittool]
bin = "sh"
params = { verbose = "bool" }
"#,
        )
        .unwrap();
        let (catalog, warnings) = AdapterCatalog::load_dir(dir.path());
        assert!(warnings.is_empty(), "{warnings:?}");

        let mut ev = Evaluator::new(dir.path().into());
        ev.set_adapters(catalog);
        let Value::Record(record) = eval_parsed(&mut ev, "which audittool") else {
            panic!("expected adapter resolution record")
        };
        assert_eq!(record.get("source"), Some(&Value::Str("adapter".into())));
        assert!(matches!(record.get("adapter"), Some(Value::Record(schema))
            if schema.get("bin") == Some(&Value::Str("sh".into()))));
        assert!(
            matches!(record.get("executable"), Some(Value::Record(executable))
            if executable.get("path").is_some_and(|path| !matches!(path, Value::Null)))
        );
        assert!(matches!(record.get("hash"), Some(Value::Str(hash)) if !hash.is_empty()));
    }

    /// Fix 2: two scopes constraining `faketool` incompatibly is a pure
    /// manifest-chain decision (site/content/internals/reef-resolution.md) — no real tool install needed, so
    /// this doesn't need a fixture resolver at all. Before the fix, `which`'s
    /// `Err(_)` arm swallowed this and reported a bare ambient/null guess.
    #[test]
    fn which_surfaces_conflict_instead_of_ambient_fallback() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(".reef.toml"),
            "[tools]\nfaketool = \"18\"\n",
        )
        .unwrap();
        let sub = root.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(".reef.toml"), "[tools]\nfaketool = \"22\"\n").unwrap();

        let Value::Record(r) = parsed(&sub, "which faketool") else {
            panic!("expected a record")
        };
        assert_eq!(
            r.get("scope"),
            Some(&Value::Str("unresolved: reef_conflict".into()))
        );
        assert!(
            matches!(r.get("note"), Some(Value::Str(s)) if s.contains("18") && s.contains("22")),
            "note should cite both conflicting constraints, got {:?}",
            r.get("note")
        );
    }

    /// Fix 2: a valid-but-drifted lock entry (hand-written, pointing at a
    /// fixture file whose content doesn't match the recorded hash) is a pure
    /// function of the lock + on-disk bytes — no real provider/tool needed,
    /// since a valid lock entry short-circuits `resolve()` before any
    /// provider is ever consulted.
    #[test]
    fn which_surfaces_drift_instead_of_ambient_fallback() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".reef.toml"), "[tools]\nfaketool = \"*\"\n").unwrap();
        let bin = dir.path().join("fakebin");
        std::fs::write(&bin, b"original-bytes").unwrap();
        std::fs::write(
            dir.path().join("reef.lock"),
            format!(
                "[tool.faketool]\nname = \"faketool\"\nversion = \"1.0.0\"\nprovider = \"mise\"\npath = \"{}\"\nblake3 = \"deadbeef\"\nresolved_at = \"2026-01-01T00:00:00Z\"\n",
                bin.display()
            ),
        )
        .unwrap();

        let Value::Record(r) = parsed(dir.path(), "which faketool") else {
            panic!("expected a record")
        };
        assert_eq!(
            r.get("scope"),
            Some(&Value::Str("unresolved: reef_drift".into()))
        );
    }

    /// Fix 4: `reef doctor`'s drift check, same fixture shape as the `which`
    /// drift test above.
    #[test]
    fn reef_doctor_flags_drift() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".reef.toml"), "[tools]\nfaketool = \"*\"\n").unwrap();
        let bin = dir.path().join("fakebin");
        std::fs::write(&bin, b"original-bytes").unwrap();
        std::fs::write(
            dir.path().join("reef.lock"),
            format!(
                "[tool.faketool]\nname = \"faketool\"\nversion = \"1.0.0\"\nprovider = \"mise\"\npath = \"{}\"\nblake3 = \"deadbeef\"\nresolved_at = \"2026-01-01T00:00:00Z\"\n",
                bin.display()
            ),
        )
        .unwrap();

        let Value::Table(rows) = parsed(dir.path(), "reef doctor") else {
            panic!("expected a table")
        };
        let drift = rows
            .iter()
            .find(|r| r.get("check") == Some(&Value::Str("drift".into())))
            .expect("a drift row is present");
        assert_eq!(drift.get("name"), Some(&Value::Str("faketool".into())));
        assert_eq!(drift.get("status"), Some(&Value::Str("drift".into())));
    }

    /// Fix 4: an orphan lock entry — `reef.lock` remembers `ghosttool`, but no
    /// manifest in scope mentions it anymore.
    #[test]
    fn reef_doctor_flags_orphan_lock() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".reef.toml"), "[tools]\nsh = \"*\"\n").unwrap();
        std::fs::write(
            dir.path().join("reef.lock"),
            "[tool.ghosttool]\nname = \"ghosttool\"\nversion = \"1.0.0\"\nprovider = \"mise\"\npath = \"/nonexistent/ghosttool\"\nblake3 = \"deadbeef\"\nresolved_at = \"2026-01-01T00:00:00Z\"\n",
        )
        .unwrap();

        let Value::Table(rows) = parsed(dir.path(), "reef doctor") else {
            panic!("expected a table")
        };
        let orphan = rows
            .iter()
            .find(|r| r.get("check") == Some(&Value::Str("orphan".into())))
            .expect("an orphan row is present");
        assert_eq!(orphan.get("name"), Some(&Value::Str("ghosttool".into())));
    }

    /// Fix 4: shadowed-ambient — `sh` is locked to a fixture path, but the
    /// REAL ambient `sh` (guaranteed present on any POSIX host, same
    /// assumption the rest of this corpus/test suite already makes) resolves
    /// to a different binary.
    #[test]
    fn reef_doctor_flags_shadowed_ambient() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".reef.toml"), "[tools]\nsh = \"*\"\n").unwrap();
        let fake = dir.path().join("fake-sh");
        std::fs::write(&fake, b"not a real shell").unwrap();
        std::fs::write(
            dir.path().join("reef.lock"),
            format!(
                "[tool.sh]\nname = \"sh\"\nversion = \"1.0.0\"\nprovider = \"mise\"\npath = \"{}\"\nblake3 = \"deadbeef\"\nresolved_at = \"2026-01-01T00:00:00Z\"\n",
                fake.display()
            ),
        )
        .unwrap();

        let Value::Table(rows) = parsed(dir.path(), "reef doctor") else {
            panic!("expected a table")
        };
        let shadowed = rows
            .iter()
            .find(|r| r.get("check") == Some(&Value::Str("shadowed_ambient".into())))
            .expect("a shadowed_ambient row is present");
        assert_eq!(shadowed.get("name"), Some(&Value::Str("sh".into())));
    }

    /// The manifest filenames `ScopeChain::discover` (site/content/internals/reef-resolution.md) looks
    /// for at every directory on its walk from `cwd` up to the filesystem
    /// root.
    const REEF_MANIFEST_NAMES: &[&str] =
        &[".reef.toml", "mise.toml", ".mise.toml", ".tool-versions"];

    /// `reef_doctor_empty_scope_is_empty_table_not_error` asserts the
    /// "genuinely nothing constrains anything anywhere" invariant — but
    /// `ScopeChain::discover` walks from `dir` all the way to the real
    /// filesystem root, including the shared OS temp dir every
    /// `tempfile::tempdir()` nests under. That walk is only actually empty
    /// when no ancestor directory happens to contain a
    /// `.reef.toml`/`mise.toml`/`.mise.toml`/`.tool-versions` — true on a
    /// clean host, but not something this test can force from Rust alone
    /// (fully bounding the walk needs a root/boundary knob on
    /// `ScopeChain::discover` itself, a `shoal-reef` source change). Rather
    /// than let ambient contamination surface as a confusing generic
    /// `assertion failed` a few lines down, fail loudly here with a precise
    /// pointer at the offending file, so it reads as "environmental
    /// contamination" (fix your host / clean the shared tempdir) rather
    /// than "reef regressed".
    fn panic_if_ancestor_reef_pollution(dir: &Path) {
        let mut cur = Some(dir);
        while let Some(d) = cur {
            for name in REEF_MANIFEST_NAMES {
                let candidate = d.join(name);
                if candidate.exists() {
                    panic!(
                        "ambient reef-manifest pollution detected above this test's own \
                         tempdir: {candidate:?} exists and was NOT created by this test. \
                         ScopeChain::discover (site/content/internals/reef-resolution.md) walks from cwd to the \
                         filesystem root, so this file makes the scope chain non-empty and \
                         breaks this test's \"nothing constrains anything\" premise. This is \
                         environmental contamination (e.g. a stray manifest left in a shared \
                         /tmp by an unrelated manual `reef`/`mise` repro), not a product \
                         regression — remove the file and re-run."
                    );
                }
            }
            cur = d.parent();
        }
    }

    /// `reef doctor` with no manifest in scope is a clean, empty table — not
    /// an error (unlike `reef lock`, a health check has nothing to say about
    /// nothing).
    #[test]
    fn reef_doctor_empty_scope_is_empty_table_not_error() {
        let dir = tempfile::tempdir().unwrap();
        panic_if_ancestor_reef_pollution(dir.path());
        let Value::Table(rows) = parsed(dir.path(), "reef doctor") else {
            panic!("expected a table")
        };
        assert!(rows.is_empty());
    }

    /// Item 1 — the footgun scenario: a MALFORMED `cwd/.reef.toml` under a VALID
    /// ancestor `.reef.toml`. `reef add` must surface the LOCAL parse error and
    /// leave the ancestor's manifest byte-for-byte untouched. Before the fix,
    /// `ScopeChain::discover` silently skipped the broken local file, so the
    /// chain's nearest parsed `Reef` scope was the ANCESTOR and `reef add`
    /// silently mutated it — hiding the local parse error entirely.
    #[test]
    fn reef_add_surfaces_local_parse_error_not_ancestor_write() {
        let root = tempfile::tempdir().unwrap();
        let ancestor = root.path().join(".reef.toml");
        let ancestor_text = "[tools]\nnode = \"18\"\n";
        std::fs::write(&ancestor, ancestor_text).unwrap();
        let sub = root.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let local = sub.join(".reef.toml");
        let local_text = "[tools\nfaketool = "; // malformed TOML
        std::fs::write(&local, local_text).unwrap();

        let mut ev = Evaluator::new(sub.clone());
        let err = ev
            .eval_program(&shoal_syntax::parse("reef add faketool@1").unwrap())
            .expect_err("a malformed local manifest must surface a parse error");
        assert_eq!(err.code, "reef_provider");
        assert!(
            err.msg.contains(&local.display().to_string()),
            "the parse error must name the LOCAL manifest, got: {}",
            err.msg
        );
        // The ancestor manifest is untouched — no silent write one dir up.
        assert_eq!(std::fs::read_to_string(&ancestor).unwrap(), ancestor_text);
        // The malformed local file is left exactly as-is (we never wrote it).
        assert_eq!(std::fs::read_to_string(&local).unwrap(), local_text);
    }

    /// Item 1 — the ordinary local case: a VALID `cwd/.reef.toml` under a valid
    /// ancestor. `reef add` edits the LOCAL manifest; the ancestor is untouched.
    #[test]
    fn reef_add_edits_local_manifest_not_ancestor() {
        let root = tempfile::tempdir().unwrap();
        let ancestor = root.path().join(".reef.toml");
        let ancestor_text = "[tools]\nnode = \"18\"\n";
        std::fs::write(&ancestor, ancestor_text).unwrap();
        let sub = root.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let local = sub.join(".reef.toml");
        std::fs::write(&local, "[tools]\nrg = \"*\"\n").unwrap();

        let mut ev = Evaluator::new(sub.clone());
        // faketool never resolves, so the lock step no-ops — but the manifest
        // EDIT still lands (the tool constraint is written before the lock).
        ev.eval_program(&shoal_syntax::parse("reef add faketool@1").unwrap())
            .expect("reef add on a valid local manifest succeeds");

        let written = std::fs::read_to_string(&local).unwrap();
        let tbl: toml::Table = written.parse().unwrap();
        assert_eq!(tbl["tools"]["faketool"].as_str(), Some("1"));
        assert_eq!(tbl["tools"]["rg"].as_str(), Some("*"), "existing pin kept");
        // The ancestor never sees the new pin.
        assert_eq!(std::fs::read_to_string(&ancestor).unwrap(), ancestor_text);
    }

    /// Item 1 — no local manifest: `reef add` falls back to the chain's nearest
    /// ancestor `.reef.toml` ("writes nearest manifest", site/content/internals/reef-resolution.md), since the
    /// subdir has none of its own.
    #[test]
    fn reef_add_falls_back_to_nearest_ancestor_when_no_local() {
        let root = tempfile::tempdir().unwrap();
        let ancestor = root.path().join(".reef.toml");
        std::fs::write(&ancestor, "[tools]\nnode = \"18\"\n").unwrap();
        let sub = root.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();

        let mut ev = Evaluator::new(sub.clone());
        ev.eval_program(&shoal_syntax::parse("reef add faketool@1").unwrap())
            .expect("reef add falls back to the ancestor manifest");

        // The ancestor gained the pin; no local manifest was created.
        let tbl: toml::Table = std::fs::read_to_string(&ancestor).unwrap().parse().unwrap();
        assert_eq!(tbl["tools"]["faketool"].as_str(), Some("1"));
        assert_eq!(tbl["tools"]["node"].as_str(), Some("18"));
        assert!(
            !sub.join(".reef.toml").exists(),
            "no local manifest should be created when an ancestor exists"
        );
    }

    /// Item 1 — greenfield: no manifest anywhere in the chain → create a fresh
    /// `cwd/.reef.toml`. (Guarded against ambient ancestor pollution above the
    /// shared tempdir, which would otherwise steal the write as a "nearest
    /// ancestor".)
    #[test]
    fn reef_add_creates_local_manifest_when_none_in_scope() {
        let dir = tempfile::tempdir().unwrap();
        panic_if_ancestor_reef_pollution(dir.path());
        let mut ev = Evaluator::new(dir.path().to_path_buf());
        ev.eval_program(&shoal_syntax::parse("reef add faketool@1").unwrap())
            .expect("reef add creates a manifest when none exists");
        let local = dir.path().join(".reef.toml");
        assert!(local.exists(), "a fresh cwd/.reef.toml must be created");
        let tbl: toml::Table = std::fs::read_to_string(&local).unwrap().parse().unwrap();
        assert_eq!(tbl["tools"]["faketool"].as_str(), Some("1"));
    }
}
