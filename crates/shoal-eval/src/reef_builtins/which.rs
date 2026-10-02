//! `which` resolution and command-source reporting.

use super::*;

impl Evaluator {
    // --- `which` (site/content/internals/reef-resolution.md) -------------------------------------------------

    /// `which <tool>` → a resolution report record; `which <tool> --all` → a
    /// table of every candidate. With no manifest in scope, `which git` still
    /// finds the ambient PATH entry (a minimal report), never a regression.
    pub(crate) fn builtin_which(&mut self, call: &CmdCall) -> VResult<Value> {
        let mut names = Vec::new();
        let mut all = false;
        let mut options = true;
        for a in &call.args {
            match (options, a) {
                (true, CmdArg::DashDash { .. }) => options = false,
                (true, CmdArg::FlagLong { name, .. }) if name == "all" => {
                    if all {
                        return Err(ErrorVal::arg_error(
                            "which --all may be specified only once",
                        ));
                    }
                    all = true;
                }
                (true, CmdArg::FlagShort { chars, .. }) if chars == "a" => {
                    if all {
                        return Err(ErrorVal::arg_error(
                            "which --all/-a may be specified only once",
                        ));
                    }
                    all = true;
                }
                (true, CmdArg::FlagLong { name, .. }) => {
                    return Err(ErrorVal::arg_error(format!(
                        "which received unknown option --{}",
                        name.replace('_', "-")
                    )));
                }
                (true, CmdArg::FlagShort { chars, .. }) => {
                    return Err(ErrorVal::arg_error(format!(
                        "which received unknown option -{chars}"
                    )));
                }
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
        self.reef_lock_loaded()?;
        let resolver = self.reef_resolver();
        let mut lock = self.exec.reef.lock.clone();
        let provider_context = self.reef_provider_context(chain.cwd.clone());
        match resolver.resolve_with_probe_context(
            name,
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
                // Only keep a fresh lock when a manifest actually constrained it.
                if res.constrained {
                    if res.locked_now {
                        self.persist_reef_lock_value(&lock)?;
                    }
                    self.exec.reef.lock = lock;
                }
                report_to_record(&res.report)
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
            let mut params = OutputValues::new();
            for param in &adapter.top.params {
                params.push(Value::Str(param.name.clone()))?;
            }
            schema.insert("params".into(), params.finish_list());
            let mut subcommands = OutputValues::new();
            for subcommand in adapter.subs.keys() {
                subcommands.push(Value::Str(subcommand.clone()))?;
            }
            schema.insert("subcommands".into(), subcommands.finish_list());
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
        let mut rows = OutputValues::new();
        for provider in resolver.providers() {
            let discovery = provider.discover(name, &ctx).map_err(|error| {
                ErrorVal::new("reef_provider", error.to_string())
                    .with_hint("narrow or clean the provider's installed candidate set")
            })?;
            for cand in discovery.into_candidates() {
                let mut r = Record::new();
                r.insert("tool".into(), Value::Str(cand.tool.clone()));
                r.insert("version".into(), Value::Str(cand.version.to_string()));
                r.insert("path".into(), Value::Path(cand.path.clone()));
                r.insert("provider".into(), Value::Str(provider.name().to_string()));
                r.insert(
                    "scope".into(),
                    Value::Str(if cand.ambient { "ambient" } else { "system" }.into()),
                );
                rows.push(table_record(r))?;
            }
        }
        Ok(rows.finish_table())
    }
}
