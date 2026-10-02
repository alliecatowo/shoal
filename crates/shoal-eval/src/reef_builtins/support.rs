//! Bounded manifest reads and shared Reef report encoding.

use super::*;

pub(super) fn read_optional_reef_manifest(fs: &dyn Fs, path: &Path) -> VResult<Option<String>> {
    let reader = match fs.open_read(path) {
        Ok(reader) => reader,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(ErrorVal::new(
                "reef_provider",
                format!("reading manifest {}: {error}", path.display()),
            ));
        }
    };
    let mut bytes = Vec::with_capacity(8 * 1024);
    reader
        .take((shoal_reef::REEF_MANIFEST_MAX_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            ErrorVal::new(
                "reef_provider",
                format!("reading manifest {}: {error}", path.display()),
            )
        })?;
    if bytes.len() > shoal_reef::REEF_MANIFEST_MAX_BYTES {
        return Err(ErrorVal::new(
            "reef_provider",
            format!(
                "manifest {} exceeds the {}-byte limit",
                path.display(),
                shoal_reef::REEF_MANIFEST_MAX_BYTES
            ),
        ));
    }
    String::from_utf8(bytes).map(Some).map_err(|_| {
        ErrorVal::new(
            "reef_provider",
            format!("manifest {} is not valid UTF-8", path.display()),
        )
    })
}

/// Map a resolved [`ResolutionReport`] to the record `which` renders (site/content/internals/reef-resolution.md).
pub(super) fn report_to_record(report: &ResolutionReport) -> VResult<Value> {
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
    let mut chain = OutputValues::new();
    for decision in &report.chain {
        let mut row = Record::new();
        row.insert("scope".into(), Value::Str(decision.scope.clone()));
        row.insert("source".into(), Value::Path(decision.source.clone()));
        row.insert(
            "constraint".into(),
            decision
                .constraint
                .clone()
                .map(Value::Str)
                .unwrap_or(Value::Null),
        );
        row.insert("outcome".into(), Value::Str(decision.outcome.clone()));
        chain.push(table_record(row))?;
    }
    r.insert("chain".into(), chain.finish_table());
    Ok(Value::Record(r))
}

pub(super) fn constrained_tool_names(chain: &ScopeChain) -> VResult<Vec<String>> {
    let mut seen = HashSet::new();
    let mut names = Vec::new();
    let mut budget = OutputBudget::new();
    for scope in &chain.scopes {
        for tool in scope.manifest.tools.keys() {
            if seen.insert(tool.as_str()) {
                let value = Value::Str(tool.clone());
                budget.admit_value(&value)?;
                names.push(tool.clone());
            }
        }
    }
    names.sort();
    Ok(names)
}

/// The `which` record for a resolver error that is NOT a plain "not found"
/// (`reef_conflict`/`reef_drift`/`reef_unlocked`/`reef_provider`): the real
/// protection state, not an ambient guess. Mirrors `reef_binding_table`'s own
/// `"unresolved: {code}"` idiom so `which` and bare `reef` agree on how an
/// unresolved tool renders.
pub(super) fn unresolved_which_record(name: &str, e: &ReefError, chain: &ScopeChain) -> Value {
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
pub(super) fn minimal_which_record(name: &str, path: &Path, hash: Option<&str>) -> Value {
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
pub(super) fn short_hash(hash: &str) -> String {
    hash.chars().take(8).collect()
}

/// Coerce a command-argument value to a plain name string for reef lookups.
pub(super) fn reef_value_word(v: &Value) -> VResult<String> {
    match v {
        Value::Str(s) => Ok(s.clone()),
        Value::Path(p) => Ok(p.to_string_lossy().into_owned()),
        other => Err(ErrorVal::type_error(format!(
            "expected a tool name, found {}",
            other.type_name()
        ))),
    }
}
