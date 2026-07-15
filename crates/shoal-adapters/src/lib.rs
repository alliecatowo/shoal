//! Declarative command adapters and structured output parsers (TDD §6).
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

use shoal_value::{Record, Value, json_to_value};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterClass {
    Cli,
    Tui,
    Daemon,
    /// IO.md §2.2: a head with this class, immediately followed by `{` (or
    /// the triple-raw `'''` form) at command-head position, lexes a raw
    /// balanced-brace/triple-quoted block into `Expr::LangBlock` instead of
    /// a trailing thunk — the adapter-declarative generalization of what
    /// TDD §13.13 hardcoded for `sh { }` alone. `class = "interpreter"`
    /// implies `block = "raw"` by default (no separate field needed for
    /// the only shape v1 has). This is purely a declaration the parser/eval
    /// consult by name; it does not change how `SubSpec`/`ParamSpec`
    /// argv-binding works for any *non*-block invocation of the same tool.
    Interpreter,
}

/// IO.md §2.6 step 3: how an interpreter-class raw block's source text
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
        let mut paths = match fs::read_dir(dir) {
            Ok(xs) => xs
                .filter_map(Result::ok)
                .map(|x| x.path())
                .filter(|p| p.extension().is_some_and(|x| x == "toml"))
                .collect::<Vec<_>>(),
            Err(e) => {
                warnings.push(format!("{}: {e}", dir.display()));
                return (catalog, warnings);
            }
        };
        paths.sort();
        for path in paths {
            let src = match fs::read_to_string(&path) {
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
            let Some(cmds) = doc.get("cmd").and_then(toml::Value::as_table) else {
                warnings.push(format!("{}: missing [cmd.<name>] table", path.display()));
                continue;
            };
            for (name, raw) in cmds {
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
        "str" | "bool" | "int" | "float" | "path" | "glob" | "size" | "duration" | "time"
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

pub fn parse_output(strategy: &str, bytes: &[u8], type_hint: Option<&str>) -> Option<Value> {
    match strategy {
        "none" => None,
        "json" => serde_json::from_slice(bytes)
            .ok()
            .map(|v| json_to_value(&v)),
        "ndjson" => parse_ndjson(bytes),
        "lines" => Some(Value::List(
            text(bytes)?
                .lines()
                .map(|s| Value::Str(s.trim_end_matches('\r').into()))
                .collect(),
        )),
        "kv" => parse_kv(bytes),
        "csv" => parse_delimited(bytes, b',', type_hint),
        "tsv" => parse_delimited(bytes, b'\t', type_hint),
        "z-records" => parse_z_records(bytes, type_hint),
        "porcelain-v2" => parse_porcelain_v2(bytes),
        "cols" => parse_cols(bytes, type_hint),
        "tsv-headerless" => parse_tsv_headerless(bytes, type_hint),
        _ => None,
    }
}

fn text(bytes: &[u8]) -> Option<&str> {
    std::str::from_utf8(bytes).ok()
}

fn parse_ndjson(bytes: &[u8]) -> Option<Value> {
    let vals = text(bytes)?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).ok().map(|v| json_to_value(&v)))
        .collect::<Option<Vec<_>>>()?;
    rows_or_list(vals)
}

fn parse_kv(bytes: &[u8]) -> Option<Value> {
    let mut r = Record::new();
    for line in text(bytes)?.lines().filter(|l| !l.trim().is_empty()) {
        let (k, v) = line.split_once('=').or_else(|| line.split_once(':'))?;
        r.insert(k.trim().into(), Value::Str(v.trim().into()));
    }
    Some(Value::Record(r))
}

fn parse_delimited(bytes: &[u8], delim: u8, hint: Option<&str>) -> Option<Value> {
    let rows = delimited_rows(bytes, delim)?;
    let (header, data) = rows.split_first()?;
    let mut out = Vec::new();
    for cells in data {
        if cells.len() != header.len() {
            return None;
        }
        let schema = hint_schema(hint);
        let mut record = Record::new();
        for (name, cell) in header.iter().zip(cells) {
            let ty = schema
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, ty)| ty.as_str())
                .unwrap_or("str");
            record.insert(name.clone(), coerce_cell(cell, ty)?);
        }
        out.push(record);
    }
    Some(Value::Table(out))
}

/// RFC4180-enough parser: quoted delimiters, doubled quotes, and embedded newlines.
fn delimited_rows(bytes: &[u8], delim: u8) -> Option<Vec<Vec<String>>> {
    let s = text(bytes)?;
    let b = s.as_bytes();
    let mut rows = vec![];
    let mut row = vec![];
    let mut field = String::new();
    let mut quoted = false;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' if quoted && b.get(i + 1) == Some(&b'"') => {
                field.push('"');
                i += 1;
            }
            b'"' => quoted = !quoted,
            x if x == delim && !quoted => {
                row.push(std::mem::take(&mut field));
            }
            b'\n' if !quoted => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            b'\r' if !quoted => {}
            _ => {
                let ch = s[i..].chars().next()?;
                field.push(ch);
                i += ch.len_utf8() - 1;
            }
        }
        i += 1;
    }
    if quoted {
        return None;
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    Some(rows)
}

fn hint_schema(hint: Option<&str>) -> Vec<(String, String)> {
    let Some(body) = hint
        .and_then(|h| h.split_once("<{"))
        .and_then(|(_, x)| x.strip_suffix("}>"))
    else {
        return vec![];
    };
    body.split(',')
        .filter_map(|f| {
            f.split_once(':')
                .map(|(n, t)| (n.trim().into(), t.trim().trim_end_matches('?').into()))
        })
        .collect()
}

fn coerce_cell(raw: &str, ty: &str) -> Option<Value> {
    Some(match ty {
        "str" | "datetime" => Value::Str(raw.into()),
        "path" => Value::Path(raw.into()),
        "int" => Value::Int(raw.parse().ok()?),
        "float" => Value::Float(raw.parse().ok()?),
        "bool" => Value::Bool(raw.parse().ok()?),
        "size" => Value::Size(shoal_value::parse_size(raw)?),
        "duration" => Value::Duration(shoal_value::parse_duration(raw)?),
        "time" => Value::Time(shoal_value::parse_time(raw)?),
        _ => Value::Str(raw.into()),
    })
}

/// Parses whitespace-column tables from tools whose header line can't be
/// trusted to determine field count or names (e.g. `df`'s `Mounted on`
/// header is two words over one data column, and `ps`'s `%CPU`/`%MEM`
/// headers aren't stable identifiers across GNU/BSD `ps`). Unlike
/// `csv`/`tsv`, which look up each column by the *header text* found in the
/// bytes, `cols` always discards the first line as a header and takes
/// column identity and order entirely from the `output.type` hint
/// (positionally, like `z-records`) -- this is what lets a portable
/// `-o keyword=CustomHeader` invoke template stay decoupled from whatever
/// the underlying OS happens to print.
///
/// Each remaining line is split on runs of whitespace. A line with fewer
/// fields than the hint degrades the whole parse to `None` (mismatch, not a
/// lie). A line with *more* fields than the hint has its overflow merged
/// (space-joined) into the last column, so a last column that legitimately
/// contains embedded whitespace (a mount path with a space, a multi-word
/// process command) survives instead of desyncing every column after it.
fn parse_cols(bytes: &[u8], hint: Option<&str>) -> Option<Value> {
    let fields = hint_schema(hint);
    if fields.is_empty() {
        return None;
    }
    let body = text(bytes)?;
    let mut lines = body.lines();
    lines.next(); // header row: discarded, never consulted for shape or names
    let rows = lines
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < fields.len() {
                return None;
            }
            let last = if parts.len() > fields.len() {
                parts.split_off(fields.len() - 1).join(" ")
            } else {
                parts.pop()?.to_owned()
            };
            let mut r = Record::new();
            for (name, ty) in &fields[..fields.len() - 1] {
                r.insert(name.clone(), coerce_cell(parts.remove(0), ty)?);
            }
            let (last_name, last_ty) = &fields[fields.len() - 1];
            r.insert(last_name.clone(), coerce_cell(&last, last_ty)?);
            Some(r)
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Value::Table(rows))
}

/// Parses tab-column data from tools whose output has **no header line at
/// all** -- unlike `cols` (which always discards a first line as a header)
/// and unlike `tsv`/`csv` (which read column names from a header line
/// found in the bytes themselves). `du -h`'s `<size>\t<path>` per line is
/// exactly this shape: every line is data, with no header row to discard or
/// to read names from. This is the genuine bug this parser exists to fix
/// (Real bug #2): `adapters/du.toml` used to declare `parse = "tsv"`,
/// which treats the very FIRST real `du` line as a header — silently
/// swallowing it as fake column names (e.g. an actual size like `"4.0K"`
/// and an actual path becoming the promised `size`/`path` keys' stand-ins)
/// instead of a genuine `{size, path}` row, and losing that row from the
/// table entirely. Column identity/order instead comes entirely from the
/// `output.type` hint (positionally, like `z-records`/`cols`), and the
/// delimiter is a literal tab -- not a whitespace run like `cols` uses --
/// so a last-field value containing ordinary spaces (a path with a space in
/// it) survives without needing `cols`'s overflow-merge trick. A line that
/// doesn't split into exactly the hint's field count degrades the whole
/// parse to `None` (mismatch, not a lie), same as `csv`/`tsv`'s exact
/// column-count check.
fn parse_tsv_headerless(bytes: &[u8], hint: Option<&str>) -> Option<Value> {
    let fields = hint_schema(hint);
    if fields.is_empty() {
        return None;
    }
    let body = text(bytes)?;
    let rows = body
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() != fields.len() {
                return None;
            }
            let mut r = Record::new();
            for ((name, ty), raw) in fields.iter().zip(&parts) {
                r.insert(name.clone(), coerce_cell(raw, ty)?);
            }
            Some(r)
        })
        .collect::<Option<Vec<_>>>()?;
    Some(Value::Table(rows))
}

fn parse_z_records(bytes: &[u8], hint: Option<&str>) -> Option<Value> {
    let fields = hint_schema(hint);
    if fields.is_empty() {
        return None;
    }
    // No output at all (e.g. `git log -z` on a path with no history) is a
    // valid, empty table, not a parse failure.
    if bytes.is_empty() {
        return Some(Value::Table(Vec::new()));
    }
    let mut cells = bytes.split(|b| *b == 0).collect::<Vec<_>>();
    // A stream of nothing but NUL bytes carries no field data at all: an empty
    // table, not a row of empty fields.
    if cells.iter().all(|x| x.is_empty()) {
        return Some(Value::Table(Vec::new()));
    }
    // A well-formed NUL-*terminated* stream of N complete records splits into
    // exactly ONE trailing empty cell (the final record terminator), giving
    // `N * fields + 1` cells. Pop only that single terminator -- and only when
    // it is genuinely a stray terminator (`len % fields == 1`), NEVER a
    // legitimately-empty FINAL field. The old loop popped *every* trailing
    // empty, so a real `git log ... -z` whose most-recent commit has an empty
    // subject (`...\0author\0date\0<empty subject>\0`) lost its trailing empty
    // field, breaking `len % fields` and degrading the whole table to raw
    // bytes. Popping exactly one keeps that `subject: ""` field intact. (A
    // stream with more than one stray trailing separator is genuinely
    // malformed and degrades to bytes below, per TDD §6 "mismatch degrades to
    // bytes rather than lying" -- it can't be told apart from an empty final
    // field without lying about one of the two.)
    if cells.len() % fields.len() == 1 && cells.last().is_some_and(|x| x.is_empty()) {
        cells.pop();
    }
    if cells.is_empty() {
        return Some(Value::Table(Vec::new()));
    }
    if cells.len() % fields.len() != 0 {
        return None;
    }
    let mut rows = Vec::new();
    for chunk in cells.chunks(fields.len()) {
        let mut r = Record::new();
        for ((name, ty), raw) in fields.iter().zip(chunk) {
            r.insert(
                name.clone(),
                coerce_cell(std::str::from_utf8(raw).ok()?, ty)?,
            );
        }
        rows.push(r);
    }
    Some(Value::Table(rows))
}

/// Parses `git status --porcelain=v2` records (see git-status(1)):
///
///   `? <path>`                                                     untracked
///   `! <path>`                                                     ignored
///   `1 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <path>`                  ordinary (9 fields)
///   `2 <XY> <sub> <mH> <mI> <mW> <hH> <hI> <X><score> <path>\t<orig>` renamed/copied (10 fields)
///
/// Every shape is validated exactly, not assumed: a line that merely starts
/// with the right marker byte but doesn't match the rest of the shape
/// (including the *real* bug this guards against — short-format `"?? path"`
/// lines, which put a second marker character where porcelain v2 puts a
/// space, silently shifting the path slice by one byte) degrades the whole
/// parse to `None` instead of emitting a `path` with corrupted bytes baked
/// in. Unmerged (`u`) records and any other unrecognized non-comment line
/// degrade the same way, since this adapter does not model their shape and
/// silently dropping them would misrepresent the status as complete. Per
/// TDD §6: "mismatch degrades to bytes + warning rather than lying."
///
/// Beyond the raw `status` field (porcelain's two-character `XY` code for
/// `1`/`2` rows, or the bare `?`/`!` marker), every row also gets a semantic
/// `state: str` so `(git status).where(.state == "modified")` reads
/// naturally instead of requiring callers to know porcelain's XY alphabet
/// (see `xy_state` for the `1`/`2` mapping). `?`/`!` rows get the fixed
/// `"untracked"`/`"ignored"` state directly, since they have no `XY` pair to
/// derive from. `status` itself is unchanged/untouched by this — existing
/// consumers of the raw code keep working exactly as before.
fn parse_porcelain_v2(bytes: &[u8]) -> Option<Value> {
    let mut rows = Vec::new();
    for line in text(bytes)?.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let raw = line.as_bytes();
        let mut r = Record::new();
        match raw[0] {
            b'?' | b'!' => {
                // Must be exactly `<marker><space><path>`. A short-format
                // line has a second marker byte at index 1 instead of a
                // space, which is exactly how a leading space used to get
                // baked into the path -- refuse to slice unless the
                // separator is genuinely there.
                if raw.get(1) != Some(&b' ') || line.len() < 3 {
                    return None;
                }
                let state = if raw[0] == b'?' {
                    "untracked"
                } else {
                    "ignored"
                };
                r.insert("status".into(), Value::Str(line[..1].into()));
                r.insert("state".into(), Value::Str(state.into()));
                r.insert("path".into(), Value::Path(line[2..].into()));
            }
            b'1' => {
                let parts: Vec<&str> = line.splitn(9, ' ').collect();
                if parts.len() != 9 {
                    return None;
                }
                r.insert("status".into(), Value::Str(parts[1].into()));
                r.insert("state".into(), Value::Str(xy_state(parts[1]).into()));
                r.insert("path".into(), Value::Path(parts[8].into()));
            }
            b'2' => {
                let parts: Vec<&str> = line.splitn(10, ' ').collect();
                if parts.len() != 10 {
                    return None;
                }
                r.insert("status".into(), Value::Str(parts[1].into()));
                r.insert("state".into(), Value::Str(xy_state(parts[1]).into()));
                let (path, orig) = match parts[9].split_once('\t') {
                    Some((p, o)) => (p, Some(o)),
                    None => (parts[9], None),
                };
                r.insert("path".into(), Value::Path(path.into()));
                if let Some(orig) = orig {
                    r.insert("orig".into(), Value::Path(orig.into()));
                }
            }
            _ => return None,
        }
        rows.push(r);
    }
    Some(Value::Table(rows))
}

/// Maps a porcelain-v2 `XY` code's two characters (`X` = staged/index state,
/// `Y` = worktree state; see git-status(1)) to a single semantic state word.
///
/// Rule: the worktree half (`Y`, second char) wins if it is anything other
/// than unmodified (`.`); otherwise the staged half (`X`, first char) is
/// used. Concretely this means a file changed in the worktree — whether or
/// not it's *also* staged (`.M` or `MM`) — reads as `"modified"`, while a
/// file that is staged-only with no further worktree change (`A.`) reads as
/// `"added"`, not `"unmodified"`. Each letter maps to a word: `M` ->
/// `"modified"`, `A` -> `"added"`, `D` -> `"deleted"`, `R` -> `"renamed"`,
/// `C` -> `"copied"`, `T` -> `"typechange"`, `U` -> `"unmerged"`. A missing
/// or unrecognized char (including `.`/`.`, i.e. truly no change) falls back
/// to `"unmodified"`; real git never emits a no-op `..` row, so this
/// fallback is a defensive default rather than an expected case.
fn xy_state(xy: &str) -> &'static str {
    fn word(c: char) -> &'static str {
        match c {
            'M' => "modified",
            'A' => "added",
            'D' => "deleted",
            'R' => "renamed",
            'C' => "copied",
            'T' => "typechange",
            'U' => "unmerged",
            _ => "unmodified",
        }
    }
    let mut chars = xy.chars();
    let x = chars.next().unwrap_or('.');
    let y = chars.next().unwrap_or('.');
    if y != '.' { word(y) } else { word(x) }
}

fn rows_or_list(vals: Vec<Value>) -> Option<Value> {
    if vals.iter().all(|v| matches!(v, Value::Record(_))) {
        Some(Value::Table(
            vals.into_iter()
                .map(|v| {
                    if let Value::Record(r) = v {
                        r
                    } else {
                        unreachable!()
                    }
                })
                .collect(),
        ))
    } else {
        Some(Value::List(vals))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn loads_catalog_and_survives_bad_file() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("git.toml"),
            r#"[cmd.git]
bin="git"
class="cli"
ok_codes=[0,1]
[cmd.git.sub.status]
params={short="bool", n="int?"}
positional=["n"]
flags={short={s="short"}}
effects=["fs.read(cwd)"]
output={parse="porcelain-v2", type="table<{status: str, path: path}>"}
"#,
        )
        .unwrap();
        fs::write(d.path().join("bad.toml"), "[[[").unwrap();
        let (c, warnings) = AdapterCatalog::load_dir(d.path());
        assert_eq!(warnings.len(), 1);
        let git = c.lookup("git").unwrap();
        assert_eq!(git.ok_codes, [0, 1]);
        assert_eq!(git.subs["status"].short_flags["s"], "short");
    }
    #[test]
    fn parses_json_ndjson_and_lines() {
        assert!(matches!(
            parse_output("json", br#"[{"a":1}]"#, None),
            Some(Value::Table(_))
        ));
        assert!(
            matches!(parse_output("ndjson", b"{\"a\":1}\n{\"a\":2}\n", None), Some(Value::Table(t)) if t.len()==2)
        );
        assert_eq!(
            parse_output("lines", b"a\r\nb\n", None),
            Some(Value::List(vec![
                Value::Str("a".into()),
                Value::Str("b".into())
            ]))
        );
    }
    #[test]
    fn parses_csv_quotes_and_z_records() {
        let v = parse_output(
            "csv",
            b"name,note,n\nfoo,\"a,b\",42\n",
            Some("table<{name: str, note: str, n: int}>"),
        )
        .unwrap();
        assert!(
            matches!(v, Value::Table(t) if t[0]["note"] == Value::Str("a,b".into()) && t[0]["n"] == Value::Int(42))
        );
        let h = "table<{hash: str, author: str, path: path}>";
        assert!(
            matches!(parse_output("z-records", b"abc\0Allie\0a.rs\0def\0Bob\0b.rs\0", Some(h)), Some(Value::Table(t)) if t.len()==2 && matches!(t[0]["path"], Value::Path(_)))
        );
    }
    #[test]
    fn parses_kv_and_porcelain() {
        assert!(
            matches!(parse_output("kv", b"a=1\nb: two\n", None), Some(Value::Record(r)) if r.len()==2)
        );
        let p = parse_output(
            "porcelain-v2",
            b"? new file.txt\n1 .M N... 100644 100644 100644 a b src/lib.rs\n",
            None,
        );
        assert!(matches!(p, Some(Value::Table(t)) if t.len()==2));
    }
    #[test]
    fn malformed_structured_output_degrades() {
        assert!(parse_output("json", b"no", None).is_none());
        assert!(parse_output("csv", b"a,b\n1\n", None).is_none());
        assert!(parse_output("z-records", b"a\0", None).is_none());
    }

    // Regression for Real bug #2: `adapters/du.toml` used to declare
    // `parse = "tsv"` for `du -k`-shaped output, but real `du` prints NO
    // header line at all -- every line is a genuine `<size_kb>\t<path>`
    // data row. `tsv`'s "first row is the header" rule silently swallowed
    // the very first real row as fake column names (keyed by whatever that
    // row's literal text happened to be, not by the promised
    // `size_kb`/`path` hint names) and, for a single-directory invocation
    // (exactly one output line), degraded a real one-row result to a
    // phantom EMPTY table. `tsv-headerless` fixes this: every line is data,
    // and column identity/order come from the hint alone. (Bare block
    // counts, not `size`-typed: `du -k`'s numbers carry no unit suffix, the
    // same reason `df.toml`'s `size_kb`/`used_kb`/`avail_kb` are `int`, not
    // `size` -- `du -h`'s human-readable suffixes like `"310G"`/`"60K"`
    // aren't accepted by `shoal_value::parse_size` either, which only
    // recognizes `b`/`kb`/`mb`/.../`kib`/... suffixes, not bare
    // single-letter ones, so `du.toml` pins `-k` and consumes
    // `human_readable` — see that file.)
    #[test]
    fn tsv_headerless_parses_du_shaped_output_with_no_header_row() {
        let h = "table<{size_kb: int, path: path}>";
        // Single-directory `du -k .` output: exactly one line, no header.
        // The old `tsv` strategy would have swallowed this as a "header"
        // and returned an empty table.
        let single = parse_output("tsv-headerless", b"335328176\t.\n", Some(h)).unwrap();
        assert_eq!(
            single,
            Value::Table(vec![{
                let mut r = Record::new();
                r.insert("size_kb".into(), Value::Int(335328176));
                r.insert("path".into(), Value::Path(".".into()));
                r
            }])
        );

        // Multi-line `du -k` output: every line (including the first) must
        // survive as a real row, not get eaten as a header.
        let multi = parse_output(
            "tsv-headerless",
            b"44\tcrates/shoal-adapters/src\n24\tcrates/shoal-adapters/tests\n",
            Some(h),
        )
        .unwrap();
        let Value::Table(rows) = multi else {
            panic!("expected table")
        };
        assert_eq!(rows.len(), 2, "no row should be swallowed as a header");
        assert_eq!(rows[0]["size_kb"], Value::Int(44));
        assert_eq!(
            rows[1]["path"],
            Value::Path("crates/shoal-adapters/tests".into())
        );
    }

    #[test]
    fn tsv_headerless_preserves_embedded_spaces_via_explicit_tab_delimiter() {
        // Unlike `cols` (which splits on whitespace RUNS and must merge
        // overflow into the last column to survive embedded spaces),
        // `tsv-headerless` splits on a literal tab, so a path containing
        // ordinary spaces survives with no special-casing at all.
        let h = "table<{size_kb: int, path: path}>";
        let v = parse_output("tsv-headerless", b"512\tMy Documents\n", Some(h)).unwrap();
        let Value::Table(rows) = v else {
            panic!("expected table")
        };
        assert_eq!(rows[0]["path"], Value::Path("My Documents".into()));
    }

    #[test]
    fn tsv_headerless_degrades_on_column_count_mismatch() {
        let h = "table<{size_kb: int, path: path}>";
        // Three tab-separated fields where the hint promises exactly two.
        assert_eq!(
            parse_output("tsv-headerless", b"4\textra\tfile.txt\n", Some(h)),
            None
        );
    }

    // Regression for Real bug #1: `git status --porcelain=v2 --short` used
    // to have its `?`/`!` lines parsed as if they were still porcelain v2,
    // baking a leading space into `path` (short format has a second marker
    // byte where porcelain v2 has a separating space). The parser must now
    // refuse to slice a shape it hasn't validated and degrade instead.
    #[test]
    fn porcelain_v2_short_format_corruption_degrades_instead_of_lying() {
        // `git status --porcelain=v2 --short` for an untracked file emits
        // short-format `"?? scratch/"`, not true porcelain v2's `"? scratch/"`.
        let short_format_bytes = b"?? scratch/\n";
        let out = parse_output("porcelain-v2", short_format_bytes, None);
        assert_eq!(out, None, "must degrade, not bake a corrupted path");

        // Sanity: genuine porcelain v2 for the same file still parses cleanly
        // with no leading-space corruption.
        let real_porcelain_bytes = b"? scratch/\n";
        let out = parse_output("porcelain-v2", real_porcelain_bytes, None).unwrap();
        assert!(matches!(&out, Value::Table(t) if t.len() == 1));
        if let Value::Table(t) = out {
            assert_eq!(t[0]["path"], Value::Path("scratch/".into()));
        }
    }

    #[test]
    fn porcelain_v2_rejects_malformed_and_unknown_records() {
        // '?'/'!' line with no separating space at all.
        assert_eq!(parse_output("porcelain-v2", b"?nofile\n", None), None);
        // '1' ordinary-change line missing fields.
        assert_eq!(
            parse_output("porcelain-v2", b"1 .M N... 100644 100644 a b\n", None),
            None
        );
        // A path containing spaces is legitimate (git allows unquoted
        // filenames with embedded spaces in porcelain v2) and must parse
        // cleanly rather than being mistaken for a shape violation -- the
        // metadata fields are bounded and the final field absorbs the rest.
        assert_eq!(
            parse_output(
                "porcelain-v2",
                b"1 .M N... 100644 100644 100644 a b my file.txt\n",
                None
            )
            .map(|v| matches!(v, Value::Table(t) if t[0]["path"] == Value::Path("my file.txt".into()))),
            Some(true)
        );
        // Unmerged 'u' records and other unrecognized markers are not
        // modeled by this adapter and must not be silently dropped from an
        // otherwise "successful" table.
        assert_eq!(
            parse_output(
                "porcelain-v2",
                b"u UU N... 100644 100644 100644 100644 aaa bbb ccc conflict.rs\n",
                None
            ),
            None
        );
    }

    #[test]
    fn porcelain_v2_renamed_entry_populates_orig() {
        let bytes =
            b"2 R100 N... 100644 100644 100644 aaaa1111 bbbb2222 R100 new_name.rs\told_name.rs\n";
        let v = parse_output("porcelain-v2", bytes, None).unwrap();
        let Value::Table(rows) = v else {
            panic!("expected table")
        };
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["status"], Value::Str("R100".into()));
        assert_eq!(rows[0]["path"], Value::Path("new_name.rs".into()));
        assert_eq!(rows[0]["orig"], Value::Path("old_name.rs".into()));
    }

    // The whole point of the `state` field: `(git status).where(.status ==
    // "modified")` can never match anything because `status` is always the
    // raw two-char `XY` porcelain code (`.M`, `M.`, `MM`, ...), never an
    // English word -- a footgun for a shell whose pitch is "filter
    // structured output like data." `state` is the derived semantic word
    // that makes `.where(.state == "modified")` actually work, without
    // disturbing `status` (asserted alongside `state` on every row here so
    // a regression to either field is caught).
    #[test]
    fn porcelain_v2_derives_semantic_state_field() {
        let bytes = b"1 .M N... 100644 100644 100644 aaaa1111 bbbb2222 mod_worktree.rs\n\
1 M. N... 100644 100644 100644 aaaa1111 bbbb2222 mod_staged.rs\n\
1 A. N... 100644 100644 100644 aaaa1111 bbbb2222 added_staged.rs\n\
1 MM N... 100644 100644 100644 aaaa1111 bbbb2222 mod_both.rs\n\
2 R. N... 100644 100644 100644 aaaa1111 bbbb2222 R100 new_name.rs\told_name.rs\n\
? untracked_file.txt\n\
! ignored_file.log\n";
        let v = parse_output("porcelain-v2", bytes, None).unwrap();
        let Value::Table(rows) = v else {
            panic!("expected table")
        };
        assert_eq!(rows.len(), 7);

        // `.M`: unmodified in the index, modified in the worktree -- Y
        // wins, and Y='M' -> "modified". This is the exact case the task
        // description calls out: a worktree-modified file must read as
        // "modified", not something derived from the (unchanged) index half.
        assert_eq!(rows[0]["status"], Value::Str(".M".into()));
        assert_eq!(rows[0]["state"], Value::Str("modified".into()));

        // `M.`: modified in the index, unmodified in the worktree -- Y is
        // '.' (no worktree change to prefer), so falls back to X='M' ->
        // "modified".
        assert_eq!(rows[1]["status"], Value::Str("M.".into()));
        assert_eq!(rows[1]["state"], Value::Str("modified".into()));

        // `A.`: staged-new (added to the index) with no further worktree
        // change -- Y='.' falls back to X='A' -> "added", per the task's
        // required outcome for a purely staged-new file.
        assert_eq!(rows[2]["status"], Value::Str("A.".into()));
        assert_eq!(rows[2]["state"], Value::Str("added".into()));

        // `MM`: modified in both the index and the worktree -- Y='M' wins
        // (same result as `.M`, since worktree state takes priority
        // regardless of what's also staged).
        assert_eq!(rows[3]["status"], Value::Str("MM".into()));
        assert_eq!(rows[3]["state"], Value::Str("modified".into()));

        // `2 R.`: a rename entry with an unmodified worktree since the
        // rename -- Y='.' falls back to X='R' -> "renamed"; `orig` still
        // carries the pre-rename path exactly as before.
        assert_eq!(rows[4]["status"], Value::Str("R.".into()));
        assert_eq!(rows[4]["state"], Value::Str("renamed".into()));
        assert_eq!(rows[4]["path"], Value::Path("new_name.rs".into()));
        assert_eq!(rows[4]["orig"], Value::Path("old_name.rs".into()));

        // `?`: untracked has no `XY` pair at all -- fixed "untracked" state.
        assert_eq!(rows[5]["status"], Value::Str("?".into()));
        assert_eq!(rows[5]["state"], Value::Str("untracked".into()));

        // `!`: ignored, likewise fixed -- "ignored".
        assert_eq!(rows[6]["status"], Value::Str("!".into()));
        assert_eq!(rows[6]["state"], Value::Str("ignored".into()));
    }

    #[test]
    fn z_records_empty_output_is_empty_table() {
        let h = "table<{hash: str, author: str, path: path}>";
        assert_eq!(
            parse_output("z-records", b"", Some(h)),
            Some(Value::Table(vec![]))
        );
    }

    #[test]
    fn z_records_single_terminator_and_noise() {
        let h = "table<{hash: str, author: str, path: path}>";
        // A single trailing NUL (the normal `-z`-terminated shape): `N*fields+1`
        // cells, one trailing terminator empty -> pop exactly one -> N rows.
        let single = parse_output("z-records", b"abc\0Allie\0a.rs\0", Some(h)).unwrap();
        assert!(matches!(&single, Value::Table(t) if t.len() == 1));
        // Pure separator noise with no field data at all is an empty table.
        assert_eq!(
            parse_output("z-records", b"\0\0", Some(h)),
            Some(Value::Table(vec![]))
        );
        // Regression for FIX 3: a genuinely-empty FINAL field must survive.
        // `abc\0Allie\0\0` (path is empty) + `-z` terminator `\0` splits to
        // [abc, Allie, "", ""] = 4 cells, `4 % 3 == 1`; popping the single
        // terminator keeps the empty `path` field instead of over-trimming it
        // and degrading the whole table to bytes.
        let empty_last =
            parse_output("z-records", b"abc\0Allie\0\0", Some(h)).expect("empty last field parses");
        match &empty_last {
            Value::Table(t) => {
                assert_eq!(t.len(), 1);
                assert_eq!(t[0]["path"], Value::Path("".into()));
            }
            other => panic!("expected a 1-row table, got {other:?}"),
        }
        // A stream carrying MORE than the single record terminator (a genuine
        // stray extra NUL) is malformed and now degrades to bytes rather than
        // being silently absorbed -- the deliberate FIX 3 trade: absorbing
        // arbitrary trailing NULs is indistinguishable from an empty final
        // field, and preserving the field wins (TDD §6).
        assert!(parse_output("z-records", b"abc\0Allie\0a.rs\0\0", Some(h)).is_none());
    }

    #[test]
    fn z_records_git_log_empty_subject_stays_a_table() {
        // The real trigger: `adapters/git.toml`'s `git log ... -z` emits 4
        // fields (`%H\0%an\0%aI\0%s`) per commit. A repo whose most-recent
        // commit has an EMPTY subject produces a trailing empty `subject` cell
        // immediately before the `-z` record terminator. The old loop popped
        // both, corrupting `len % fields` and degrading the log to raw bytes;
        // FIX 3 pops only the single terminator so `subject: ""` survives.
        let h = "table<{hash: str, author: str, date: datetime, subject: str}>";
        // Two commits: the second (most-recent-first from git) has an empty
        // subject. Bytes: rec1 fields + \0, rec2 fields (empty subject) + \0.
        // `\x00` (not `\0`) before the year digits so the NUL doesn't read as
        // an octal escape.
        let bytes =
            b"h2\0Bob\x002024-01-02T00:00:00Z\0\0h1\0Alice\x002024-01-01T00:00:00Z\0first commit\0";
        let v = parse_output("z-records", bytes, Some(h)).expect("git-log z-records parses");
        match &v {
            Value::Table(t) => {
                assert_eq!(t.len(), 2, "both commits present");
                assert_eq!(t[0]["subject"], Value::Str("".into()));
                assert_eq!(t[1]["subject"], Value::Str("first commit".into()));
            }
            other => panic!("expected a 2-row table, got {other:?}"),
        }
    }

    #[test]
    fn bundled_adapter_pack_loads_without_warnings() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../adapters");
        let (catalog, warnings) = AdapterCatalog::load_dir(&root);
        assert!(
            warnings.is_empty(),
            "bundled adapter warnings: {warnings:#?}"
        );
        let required = [
            "git",
            "cargo",
            "rg",
            "docker",
            "kubectl",
            "jq",
            "curl",
            "tar",
            "fd",
            "du",
            "ps",
            "df",
            "systemctl",
            "brew",
            "npm",
            "pnpm",
            "gh",
            "go",
            "pip",
            "sqlite3",
            "terraform",
            "helm",
            "ip",
            "python",
            "node",
            "ruby",
            "deno",
            "bash",
            "ss",
            "systemd-analyze",
            "jj",
            "rustup",
            "bun",
            "aws",
            "gcloud",
            "yq",
            "stat",
            "zip",
            "unzip",
            "yarn",
            "uv",
            "podman",
        ];
        assert_eq!(catalog.len(), required.len());
        for name in required {
            assert!(catalog.lookup(name).is_some(), "missing adapter {name}");
        }
        // IO.md §2.2: the interpreter-class set the shipped pack declares,
        // wired end to end through the same loader path as every other
        // class value.
        for interp in ["python", "node", "ruby", "deno", "jq", "bash", "yq"] {
            assert_eq!(
                catalog.lookup(interp).unwrap().class,
                AdapterClass::Interpreter,
                "{interp} should be class = \"interpreter\""
            );
            // No adapter declares invoke_payload explicitly yet, so every
            // interpreter-class adapter falls back to the documented
            // default.
            assert_eq!(
                catalog.lookup(interp).unwrap().invoke_payload,
                InvokePayload::Arg
            );
        }
        // python/node/ruby/deno/bash each declare the flag template that
        // precedes their raw block's payload argv word (IO.md §2.6 step 3);
        // jq takes its filter as a bare positional, so it declares none.
        assert_eq!(
            catalog.lookup("python").unwrap().top.invoke,
            Some(vec!["-c".to_string()])
        );
        assert_eq!(
            catalog.lookup("node").unwrap().top.invoke,
            Some(vec!["-e".to_string()])
        );
        assert_eq!(
            catalog.lookup("deno").unwrap().top.invoke,
            Some(vec!["eval".to_string()])
        );
        assert_eq!(catalog.lookup("jq").unwrap().top.invoke, None);
        // `yq` (mikefarah, YAML-native jq-alike) pins `-o=json` since its
        // default output is YAML, unlike jq's already-JSON default.
        assert_eq!(
            catalog.lookup("yq").unwrap().top.invoke,
            Some(vec!["-o=json".to_string()])
        );
        // The `cols` strategy (added for `ps`/`df`) is wired end to end
        // through the same loader path as every other parser.
        assert_eq!(catalog.lookup("ps").unwrap().top.parse, "cols");
        assert_eq!(catalog.lookup("df").unwrap().top.parse, "cols");
        // The `tsv-headerless` strategy (added for `du`/`stat`, Real bug #2)
        // is wired end to end through the same loader path as every other
        // parser, and `du`'s `human_readable` flag is consumed to protect
        // the pinned `-k` numeric format (see `du.toml`).
        assert_eq!(catalog.lookup("du").unwrap().top.parse, "tsv-headerless");
        assert_eq!(catalog.lookup("stat").unwrap().top.parse, "tsv-headerless");
        assert_eq!(
            catalog.lookup("du").unwrap().top.consumed,
            vec!["human_readable".to_string()]
        );
        // gh's two-word real subcommands are flattened into single
        // shoal-side sub names whose `invoke` template supplies both words.
        assert_eq!(
            catalog.lookup("gh").unwrap().subs["pr_list"].invoke,
            Some(vec![
                "pr".to_string(),
                "list".to_string(),
                "--json".to_string(),
                "number,title,state,author,url,createdAt".to_string()
            ])
        );
        assert_eq!(
            catalog.lookup("cargo").unwrap().subs["metadata"].invoke,
            Some(vec![
                "metadata".to_string(),
                "--format-version".to_string(),
                "1".to_string()
            ])
        );
        assert_eq!(
            catalog.lookup("git").unwrap().subs["diff"].ok_codes,
            Some(vec![0, 1])
        );
        assert_eq!(catalog.lookup("rg").unwrap().top.parse, "ndjson");
        assert_eq!(
            catalog.lookup("docker").unwrap().class,
            AdapterClass::Daemon
        );
        // The porcelain-corruption fix: `short`/`branch` stay valid,
        // declared flags but must never reach git's argv alongside the
        // pinned `--porcelain=v2` invoke template.
        let git_status = &catalog.lookup("git").unwrap().subs["status"];
        assert!(git_status.params.iter().any(|p| p.name == "short"));
        assert!(git_status.short_flags.contains_key("s"));
        assert_eq!(
            git_status.consumed,
            vec!["short".to_string(), "branch".to_string()]
        );
        // Same class of fix, swept into docker's format-pinned subs.
        let docker = catalog.lookup("docker").unwrap();
        assert_eq!(docker.subs["ps"].consumed, vec!["quiet".to_string()]);
        assert_eq!(docker.subs["images"].consumed, vec!["quiet".to_string()]);
        // kubectl's `get` and rg's top-level command pin an output format
        // too, but declare no forwardable param that could override it, so
        // there is nothing to consume there.
        assert!(
            catalog.lookup("kubectl").unwrap().subs["get"]
                .consumed
                .is_empty()
        );
        assert!(catalog.lookup("rg").unwrap().top.consumed.is_empty());
        // git's flattened `stash_list`/`stash_push`/`stash_pop` subs (no
        // single real verb, same trick as `gh`'s `pr_list`/`run_list`).
        let git = catalog.lookup("git").unwrap();
        assert_eq!(
            git.subs["stash_list"].invoke,
            Some(vec!["stash".to_string(), "list".to_string()])
        );
        assert_eq!(
            git.subs["stash_push"].invoke,
            Some(vec!["stash".to_string(), "push".to_string()])
        );
        assert_eq!(
            git.subs["stash_pop"].invoke,
            Some(vec!["stash".to_string(), "pop".to_string()])
        );
        assert!(git.subs.contains_key("show"));
        assert!(git.subs.contains_key("remote"));
        // podman's own formatter recognizes bare `--format json` directly
        // (no `docker.toml`-style go-template/tsv workaround needed).
        assert_eq!(
            catalog.lookup("podman").unwrap().subs["ps"].invoke,
            Some(vec![
                "ps".to_string(),
                "--format".to_string(),
                "json".to_string()
            ])
        );
        assert_eq!(
            catalog.lookup("podman").unwrap().subs["ps"].consumed,
            vec!["quiet".to_string()]
        );
    }

    #[test]
    fn invalid_schema_warns_without_poisoning_siblings() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("pack.toml"),
            r#"
[cmd.good]
params={path="path"}
[cmd.bad_type]
params={x="quantum"}
[cmd.bad_parser]
output={parse="wishful"}
[cmd.bad_binding]
params={x="str"}
positional=["missing"]
"#,
        )
        .unwrap();
        let (catalog, warnings) = AdapterCatalog::load_dir(d.path());
        assert!(catalog.lookup("good").is_some());
        assert!(catalog.lookup("bad_type").is_none());
        assert!(catalog.lookup("bad_parser").is_none());
        assert!(catalog.lookup("bad_binding").is_none());
        assert_eq!(warnings.len(), 3);
    }

    #[test]
    fn consumed_targeting_undeclared_param_warns_without_poisoning_siblings() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("pack.toml"),
            r#"
[cmd.good]
params={path="path"}
[cmd.bad_consumed]
params={x="str"}
consumed=["missing"]
"#,
        )
        .unwrap();
        let (catalog, warnings) = AdapterCatalog::load_dir(d.path());
        assert!(catalog.lookup("good").is_some());
        assert!(catalog.lookup("bad_consumed").is_none());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("consumed"), "{warnings:?}");
    }

    // IO.md §2.2/§2.6: `class = "interpreter"` is a schema value alongside
    // cli|tui|daemon, and `invoke_payload` is only meaningful there.
    #[test]
    fn interpreter_class_loads_and_defaults_invoke_payload_to_arg() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("pack.toml"),
            r#"
[cmd.py]
bin="python3"
class="interpreter"
invoke=["-c"]
"#,
        )
        .unwrap();
        let (catalog, warnings) = AdapterCatalog::load_dir(d.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        let py = catalog.lookup("py").unwrap();
        assert_eq!(py.class, AdapterClass::Interpreter);
        assert_eq!(py.invoke_payload, InvokePayload::Arg);
        assert_eq!(py.top.invoke, Some(vec!["-c".to_string()]));
    }

    #[test]
    fn interpreter_class_accepts_explicit_stdin_payload_mode() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("pack.toml"),
            r#"
[cmd.example]
bin="example"
class="interpreter"
invoke_payload="stdin"
"#,
        )
        .unwrap();
        let (catalog, warnings) = AdapterCatalog::load_dir(d.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(
            catalog.lookup("example").unwrap().invoke_payload,
            InvokePayload::Stdin
        );
    }

    #[test]
    fn invoke_payload_on_non_interpreter_class_warns_without_poisoning_siblings() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("pack.toml"),
            r#"
[cmd.good]
params={path="path"}
[cmd.bad_class]
class="cli"
invoke_payload="stdin"
"#,
        )
        .unwrap();
        let (catalog, warnings) = AdapterCatalog::load_dir(d.path());
        assert!(catalog.lookup("good").is_some());
        assert!(catalog.lookup("bad_class").is_none());
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("invoke_payload"), "{warnings:?}");
    }

    #[test]
    fn unknown_invoke_payload_value_warns_without_poisoning_siblings() {
        let d = tempfile::tempdir().unwrap();
        fs::write(
            d.path().join("pack.toml"),
            r#"
[cmd.good]
params={path="path"}
[cmd.bad_payload]
class="interpreter"
invoke_payload="socket"
"#,
        )
        .unwrap();
        let (catalog, warnings) = AdapterCatalog::load_dir(d.path());
        assert!(catalog.lookup("good").is_some());
        assert!(catalog.lookup("bad_payload").is_none());
        assert_eq!(warnings.len(), 1);
    }
}
