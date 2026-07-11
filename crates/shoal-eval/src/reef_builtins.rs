//! `which`/`reef` builtins layered on reef resolution (docs/REEF.md §6).
//!
//! Split out of [`crate::reef`] (see that module's doc for the split
//! rationale); see [`crate::reef_resolve`] for the scope-chain/resolver
//! mechanics these commands call into.

use super::*;

use shoal_reef::{ManifestKind, Policy, ProviderCtx, ResolutionReport};

impl Evaluator {
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
}

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
                d.constraint.clone().map(Value::Str).unwrap_or(Value::Null),
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
