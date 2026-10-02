//! Reef manifest, lock, fetch, and doctor commands.

use super::*;

impl Evaluator {
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
        let names = constrained_tool_names(&chain)?;
        let resolver = self.reef_resolver();
        let original_lock = self.exec.reef.lock.clone();
        let mut lock = original_lock.clone();
        let mut rows = OutputValues::new();
        let provider_context = self.reef_provider_context(chain.cwd.clone());
        for name in names {
            let mut r = Record::new();
            r.insert("name".into(), Value::Str(name.clone()));
            match resolver.resolve_with_probe_context(
                &name,
                &chain,
                &mut lock,
                Policy::Interactive,
                &mut |_| {},
                ProbeExecution {
                    guard: &mut |candidate| self.reef_probe_guard(candidate),
                    context: &provider_context,
                },
            ) {
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
            rows.push(table_record(r))?;
        }
        if lock != original_lock {
            self.persist_reef_lock_value(&lock)?;
        }
        self.exec.reef.lock = lock;
        Ok(rows.finish_table())
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
        let mut doc = match read_optional_reef_manifest(self.host.fs.as_ref(), &manifest_path)? {
            Some(text) => {
                shoal_reef::ReefManifest::parse_reef(&text).map_err(|error| {
                    ErrorVal::new(
                        "reef_provider",
                        format!("parsing manifest {}: {error}", manifest_path.display()),
                    )
                })?;
                text.parse::<toml::Table>().map_err(|e| {
                    ErrorVal::new(
                        "reef_provider",
                        format!("parsing manifest {}: {e}", manifest_path.display()),
                    )
                })?
            }
            None => toml::Table::new(),
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
        let manifest_text = doc.to_string();
        if manifest_text.len() > shoal_reef::REEF_MANIFEST_MAX_BYTES {
            return Err(ErrorVal::new(
                "reef_provider",
                format!(
                    "updated manifest {} exceeds the {}-byte limit",
                    manifest_path.display(),
                    shoal_reef::REEF_MANIFEST_MAX_BYTES
                ),
            ));
        }
        self.host
            .fs
            .atomic_replace(&manifest_path, manifest_text.as_bytes())
            .map_err(|e| ErrorVal::new("reef_provider", format!("writing manifest: {e}")))?;

        // Re-discover so the fresh constraint is in scope, then lock it.
        self.exec.reef.chain = None;
        let chain = self.reef_chain_snapshot();
        let resolver = self.reef_resolver();
        let mut lock = self.exec.reef.lock.clone();
        let mut r = Record::new();
        r.insert("added".into(), Value::Str(format!("{tool}@{ver}")));
        r.insert("manifest".into(), Value::Path(manifest_path.clone()));
        let provider_context = self.reef_provider_context(chain.cwd.clone());
        match resolver.refresh_lock_with_probe_context(
            &tool,
            &chain,
            &mut lock,
            &mut |_| {},
            &mut |candidate| self.reef_probe_guard(candidate),
            &provider_context,
        ) {
            Ok(res) => match self.persist_reef_lock_value(&lock) {
                Ok(()) => {
                    self.exec.reef.lock = lock;
                    r.insert("version".into(), Value::Str(res.version.to_string()));
                    r.insert("path".into(), Value::Path(res.path.clone()));
                    r.insert("locked".into(), Value::Bool(true));
                }
                Err(error) => {
                    r.insert("locked".into(), Value::Bool(false));
                    r.insert("note".into(), Value::Str(error.to_string()));
                }
            },
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
        let names = constrained_tool_names(&chain)?;
        let resolver = self.reef_resolver();
        let mut lock = self.exec.reef.lock.clone();
        let mut rows = OutputValues::new();
        let provider_context = self.reef_provider_context(chain.cwd.clone());
        for name in names {
            let mut r = Record::new();
            r.insert("name".into(), Value::Str(name.clone()));
            let res = if refresh {
                resolver.refresh_lock_with_probe_context(
                    &name,
                    &chain,
                    &mut lock,
                    &mut |_| {},
                    &mut |candidate| self.reef_probe_guard(candidate),
                    &provider_context,
                )
            } else {
                resolver.resolve_with_probe_context(
                    &name,
                    &chain,
                    &mut lock,
                    Policy::Interactive,
                    &mut |_| {},
                    ProbeExecution {
                        guard: &mut |candidate| self.reef_probe_guard(candidate),
                        context: &provider_context,
                    },
                )
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
            rows.push(table_record(r))?;
        }
        self.persist_reef_lock_value(&lock)?;
        self.exec.reef.lock = lock;
        Ok(rows.finish_table())
    }

    /// `reef fetch <tool>`: delegate to the tool's provider(s); may no-op when
    /// no provider can install.
    fn reef_fetch(&mut self, tool: Option<&str>) -> VResult<Value> {
        let tool = tool.ok_or_else(|| ErrorVal::arg_error("reef fetch expects a tool name"))?;
        self.reef_fetch_guard()?;
        let chain = self.reef_chain_snapshot();
        let requirement = chain
            .nearest_for(tool)
            .map(|scope| &scope.manifest.tools[tool]);
        let constraint = requirement
            .map(|requirement| requirement.constraint.clone())
            .unwrap_or(shoal_reef::Constraint::Any);
        let provider_pin = requirement.and_then(|requirement| requirement.provider.as_deref());
        let resolver = self.reef_resolver();
        let ctx = self.reef_provider_context(self.exec.shell.cwd.clone());
        let mut r = Record::new();
        r.insert("tool".into(), Value::Str(tool.to_string()));
        for provider in resolver.providers() {
            if provider_pin.is_some_and(|pin| provider.name() != pin) {
                continue;
            }
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
            Value::Str(match provider_pin {
                Some(pin) => format!("pinned provider `{pin}` cannot fetch this tool"),
                None => "no provider can fetch this tool".into(),
            }),
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
        if let Some(error) = self.exec.reef.lock_load_error.clone() {
            let mut row = Record::new();
            row.insert("name".into(), Value::Str("reef.lock".into()));
            row.insert("check".into(), Value::Str("lockfile".into()));
            row.insert("status".into(), Value::Str("invalid".into()));
            row.insert("note".into(), Value::Str(error));
            let mut rows = OutputValues::new();
            rows.push(table_record(row))?;
            return Ok(rows.finish_table());
        }
        let names = constrained_tool_names(&chain)?;

        let hashes = HashCache::new();
        let mut rows = OutputValues::new();

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
            rows.push(table_record(r))?;

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
                rows.push(table_record(s))?;
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
            rows.push(table_record(r))?;
        }

        Ok(rows.finish_table())
    }
}
