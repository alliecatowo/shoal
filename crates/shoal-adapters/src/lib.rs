//! Declarative command adapters and structured output parsers (site/content/internals/language-conformance-contract.md).
//!
//! ## The "consumed" rule (pinned-format subs)
//!
//! When a `[cmd.<x>.sub.<y>]` entry sets a fixed `invoke` argv template that
//! pins the child process's *output format* (e.g. `git status`'s
//! `--porcelain=v2`, `docker ps`'s `--format ...`), any declared `params`
//! whose forwarded flag would itself change that output format must be
//! listed in that sub's `consumed = [...]`.
//!
//! Argv is built as `invoke-template ++ user-supplied flags` (last flag
//! wins for most CLIs' format switches), so a forwardable flag that also
//! selects an output format silently overrides the pinned one downstream —
//! the parser then reads bytes in the wrong shape and can bake corruption
//! straight into a structured value (see `git status --porcelain=v2
//! --short`: git's short format inserts an extra status character before
//! the separating space, so the porcelain-v2 parser's fixed `line[2..]`
//! path slice lands one byte too early and every path gets a leading
//! space). `consumed` params stay declared (so the flag is still
//! recognized/valid and short/long forms keep working for the user) but
//! must never be pushed onto argv — the evaluator that builds argv from a
//! `SubSpec` is expected to skip any param named in `consumed`. This is
//! honest, not silently degrading UX, precisely because the pinned
//! structured output already contains a superset of what the consumed
//! flag would otherwise reveal (e.g. porcelain-v2 conveys everything
//! `--short` shows, plus more).

#[cfg(test)]
use shoal_value::{Record, Value};
use std::collections::HashMap;
#[cfg(test)]
use std::fs;
use std::path::Path;

mod catalog_input;
mod output;

pub use catalog_input::{
    MAX_ADAPTER_CATALOG_COMMANDS, MAX_ADAPTER_MANIFEST_BYTES, MAX_ADAPTER_MANIFEST_FILES,
    MAX_ADAPTER_MANIFEST_NODES, MAX_ADAPTER_MANIFEST_STRING_BYTES, MAX_ADAPTER_TOML_NESTING,
};

pub use output::{
    MAX_PARSE_CELL_BYTES, MAX_PARSE_CELLS, MAX_PARSE_COLUMNS, MAX_PARSE_HINT_BYTES,
    MAX_PARSE_INPUT_BYTES, MAX_PARSE_JSON_DEPTH, MAX_PARSE_JSON_NODES, MAX_PARSE_RETAINED_BYTES,
    MAX_PARSE_ROWS, parse_output,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterClass {
    Cli,
    Tui,
    Daemon,
    /// site/content/internals/values-streams-execution.md: a head with this class, immediately followed by `{` (or
    /// the triple-raw `'''` form) at command-head position, lexes a raw
    /// balanced-brace/triple-quoted block into `Expr::LangBlock` instead of
    /// a trailing thunk — the adapter-declarative generalization of what
    /// site/content/internals/language-conformance-contract.md hardcoded for `sh { }` alone. `class = "interpreter"`
    /// implies `block = "raw"` by default (no separate field needed for
    /// the only shape v1 has). This is purely a declaration the parser/eval
    /// consult by name; it does not change how `SubSpec`/`ParamSpec`
    /// argv-binding works for any *non*-block invocation of the same tool.
    Interpreter,
}

/// How an interpreter-class raw block's source text
/// reaches the child process. `Arg` (the default) appends it as a single
/// argv word after `top.invoke`'s flag template (e.g. `python3 -c BODY`,
/// where `invoke = ["-c"]`); `Stdin` pipes it to the child's stdin instead.
/// Declaring this on a non-`interpreter`-class adapter is a schema error
/// (the field is meaningless there) and is rejected at load time, not
/// silently ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvokePayload {
    Arg,
    Stdin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamSpec {
    pub name: String,
    pub ty: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubSpec {
    pub params: Vec<ParamSpec>,
    pub positional: Vec<String>,
    pub short_flags: HashMap<String, String>,
    pub invoke: Option<Vec<String>>,
    /// Param names that must be recognized as valid flags but never
    /// forwarded into argv. See the module-level "consumed" rule doc.
    pub consumed: Vec<String>,
    pub parse: String,
    pub output_type: Option<String>,
    pub effects: Vec<String>,
    pub ok_codes: Option<Vec<i32>>,
}

#[derive(Debug, Clone)]
pub struct CmdAdapter {
    pub name: String,
    pub bin: String,
    pub class: AdapterClass,
    pub ok_codes: Vec<i32>,
    /// Only meaningful when `class == AdapterClass::Interpreter`; see
    /// `InvokePayload`. Defaults to `Arg` for every other class.
    pub invoke_payload: InvokePayload,
    pub top: SubSpec,
    pub subs: HashMap<String, SubSpec>,
}

#[derive(Debug, Clone, Default)]
pub struct AdapterCatalog {
    cmds: HashMap<String, CmdAdapter>,
}

impl AdapterCatalog {
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load all TOML files in a directory. A malformed file or command becomes a
    /// warning; valid siblings remain available.
    pub fn load_dir(dir: &Path) -> (Self, Vec<String>) {
        let mut catalog = Self::empty();
        let mut warnings = Vec::new();
        let mut paths = match catalog_input::manifest_paths(dir) {
            Ok((paths, omitted)) => {
                if omitted > 0 {
                    warnings.push(format!(
                        "{}: {omitted} adapter manifests omitted above the {}-file limit",
                        dir.display(),
                        MAX_ADAPTER_MANIFEST_FILES
                    ));
                }
                paths
            }
            Err(e) => {
                warnings.push(format!("{}: {e}", dir.display()));
                return (catalog, warnings);
            }
        };
        paths.sort();
        for path in paths {
            let src = match catalog_input::read_manifest(&path) {
                Ok(s) => s,
                Err(e) => {
                    warnings.push(format!("{}: {e}", path.display()));
                    continue;
                }
            };
            let doc: toml::Value = match toml::from_str(&src) {
                Ok(v) => v,
                Err(e) => {
                    warnings.push(format!("{}: {e}", path.display()));
                    continue;
                }
            };
            if let Err(error) = catalog_input::validate_document(&doc) {
                warnings.push(format!("{}: {error}", path.display()));
                continue;
            }
            let Some(cmds) = doc.get("cmd").and_then(toml::Value::as_table) else {
                warnings.push(format!("{}: missing [cmd.<name>] table", path.display()));
                continue;
            };
            for (name, raw) in cmds {
                if !catalog.cmds.contains_key(name)
                    && catalog.cmds.len() >= MAX_ADAPTER_CATALOG_COMMANDS
                {
                    warnings.push(format!(
                        "{}: cmd.{name}: catalog command limit reached ({MAX_ADAPTER_CATALOG_COMMANDS})",
                        path.display()
                    ));
                    continue;
                }
                match parse_cmd(name, raw) {
                    Ok(cmd) => {
                        if catalog.cmds.insert(name.clone(), cmd).is_some() {
                            warnings.push(format!(
                                "{}: duplicate adapter cmd.{name}; later file wins",
                                path.display()
                            ));
                        }
                    }
                    Err(e) => warnings.push(format!("{}: cmd.{name}: {e}", path.display())),
                }
            }
        }
        (catalog, warnings)
    }

    pub fn lookup(&self, head: &str) -> Option<&CmdAdapter> {
        self.cmds.get(head)
    }

    /// Overlay another catalog onto this one. Commands from `later` replace
    /// commands with the same name, matching config's documented directory
    /// precedence. The returned names are the commands that were replaced.
    pub fn overlay(&mut self, later: &Self) -> Vec<String> {
        let mut replaced = Vec::new();
        for (name, command) in &later.cmds {
            if self.cmds.insert(name.clone(), command.clone()).is_some() {
                replaced.push(name.clone());
            }
        }
        replaced.sort();
        replaced
    }

    /// The command heads this catalog knows. Order is unspecified — the sole
    /// caller (the evaluator's command did-you-mean, site/content/internals/language-conformance-contract.md) sorts/dedups
    /// across candidate sources — so this is just a cheap read view over the
    /// registered names, no allocation.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.cmds.keys().map(String::as_str)
    }

    /// Adapter heads that opt into raw interpreter-block parsing. Hosts feed
    /// this exact set into `shoal_syntax::ParseCtx`, so configured adapters are
    /// grammar-reachable instead of merely carrying inert class metadata.
    pub fn interpreter_names(&self) -> impl Iterator<Item = &str> {
        self.cmds
            .iter()
            .filter(|(_, command)| command.class == AdapterClass::Interpreter)
            .map(|(name, _)| name.as_str())
    }

    pub fn len(&self) -> usize {
        self.cmds.len()
    }
    pub fn is_empty(&self) -> bool {
        self.cmds.is_empty()
    }
}

fn parse_cmd(name: &str, raw: &toml::Value) -> Result<CmdAdapter, String> {
    let t = raw.as_table().ok_or("must be a table")?;
    let bin = string(t.get("bin")).unwrap_or(name).to_owned();
    let class = match string(t.get("class")).unwrap_or("cli") {
        "cli" => AdapterClass::Cli,
        "tui" => AdapterClass::Tui,
        "daemon" => AdapterClass::Daemon,
        "interpreter" => AdapterClass::Interpreter,
        x => return Err(format!("unknown class {x:?}")),
    };
    let invoke_payload_raw = string(t.get("invoke_payload"));
    if invoke_payload_raw.is_some() && class != AdapterClass::Interpreter {
        return Err(
            "invoke_payload may only be declared on a class = \"interpreter\" adapter".into(),
        );
    }
    let invoke_payload = match invoke_payload_raw.unwrap_or("arg") {
        "arg" => InvokePayload::Arg,
        "stdin" => InvokePayload::Stdin,
        x => return Err(format!("unknown invoke_payload {x:?}")),
    };
    let ok_codes = ints(t.get("ok_codes")).unwrap_or_else(|| vec![0]);
    let top = parse_sub(t)?;
    let mut subs = HashMap::new();
    if let Some(st) = t.get("sub").and_then(toml::Value::as_table) {
        for (subname, sub) in st {
            subs.insert(
                subname.clone(),
                parse_sub(sub.as_table().ok_or("subcommand must be a table")?)?,
            );
        }
    }
    Ok(CmdAdapter {
        name: name.to_owned(),
        bin,
        class,
        ok_codes,
        invoke_payload,
        top,
        subs,
    })
}

fn parse_sub(t: &toml::Table) -> Result<SubSpec, String> {
    let mut s = SubSpec {
        parse: "none".into(),
        ..Default::default()
    };
    if let Some(params) = t.get("params").and_then(toml::Value::as_table) {
        for (name, ty) in params {
            let ty = ty.as_str().ok_or("parameter type must be a string")?;
            if !valid_type(ty) {
                return Err(format!("unknown parameter type {ty:?}"));
            }
            s.params.push(ParamSpec {
                name: name.clone(),
                ty: ty.into(),
            });
        }
    }
    if let Some(xs) = strings(t.get("positional")) {
        s.positional = xs;
    }
    if let Some(short) = t
        .get("flags")
        .and_then(|x| x.get("short"))
        .and_then(toml::Value::as_table)
    {
        for (k, v) in short {
            s.short_flags.insert(
                k.clone(),
                v.as_str()
                    .ok_or("short flag target must be a string")?
                    .into(),
            );
        }
    }
    s.invoke = strings(t.get("invoke"));
    s.consumed = strings(t.get("consumed")).unwrap_or_default();
    if let Some(out) = t.get("output").and_then(toml::Value::as_table) {
        s.parse = string(out.get("parse")).unwrap_or("none").into();
        if !matches!(
            s.parse.as_str(),
            "json"
                | "ndjson"
                | "csv"
                | "tsv"
                | "z-records"
                | "porcelain-v2"
                | "cols"
                | "cols2"
                | "tsv-headerless"
                | "lines"
                | "kv"
                | "none"
        ) {
            return Err(format!("unknown output parser {:?}", s.parse));
        }
        s.output_type = string(out.get("type")).map(str::to_owned);
    }
    s.effects = strings(t.get("effects")).unwrap_or_default();
    s.ok_codes = ints(t.get("ok_codes"));
    let names = s
        .params
        .iter()
        .map(|p| p.name.as_str())
        .collect::<std::collections::HashSet<_>>();
    for positional in &s.positional {
        if !names.contains(positional.as_str()) {
            return Err(format!(
                "positional parameter {positional:?} is not declared in params"
            ));
        }
    }
    for target in s.short_flags.values() {
        if !names.contains(target.as_str()) {
            return Err(format!(
                "short flag targets undeclared parameter {target:?}"
            ));
        }
    }
    for consumed in &s.consumed {
        if !names.contains(consumed.as_str()) {
            return Err(format!("consumed names undeclared parameter {consumed:?}"));
        }
    }
    Ok(s)
}

fn valid_type(ty: &str) -> bool {
    let base = ty.strip_suffix('?').unwrap_or(ty);
    matches!(
        base,
        "str"
            | "bool"
            | "int"
            | "float"
            | "path"
            | "glob"
            | "size"
            | "size_kb"
            | "duration"
            | "time"
    ) || (base.starts_with("list<") && base.ends_with('>') && valid_type(&base[5..base.len() - 1]))
}

fn string(v: Option<&toml::Value>) -> Option<&str> {
    v?.as_str()
}
fn strings(v: Option<&toml::Value>) -> Option<Vec<String>> {
    v?.as_array()?
        .iter()
        .map(|x| x.as_str().map(str::to_owned))
        .collect::<Option<_>>()
}
fn ints(v: Option<&toml::Value>) -> Option<Vec<i32>> {
    v?.as_array()?
        .iter()
        .map(|x| x.as_integer().and_then(|n| i32::try_from(n).ok()))
        .collect::<Option<_>>()
}

#[cfg(test)]
#[path = "lib/tests.rs"]
mod tests;
