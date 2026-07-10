//! Declarative command adapters and structured output parsers (TDD §6).

use shoal_value::{Record, Value, json_to_value};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdapterClass {
    Cli,
    Tui,
    Daemon,
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
                        catalog.cmds.insert(name.clone(), cmd);
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
}

fn parse_cmd(name: &str, raw: &toml::Value) -> Result<CmdAdapter, String> {
    let t = raw.as_table().ok_or("must be a table")?;
    let bin = string(t.get("bin")).unwrap_or(name).to_owned();
    let class = match string(t.get("class")).unwrap_or("cli") {
        "cli" => AdapterClass::Cli,
        "tui" => AdapterClass::Tui,
        "daemon" => AdapterClass::Daemon,
        x => return Err(format!("unknown class {x:?}")),
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
            s.params.push(ParamSpec {
                name: name.clone(),
                ty: ty.as_str().ok_or("parameter type must be a string")?.into(),
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
    if let Some(out) = t.get("output").and_then(toml::Value::as_table) {
        s.parse = string(out.get("parse")).unwrap_or("none").into();
        s.output_type = string(out.get("type")).map(str::to_owned);
    }
    s.effects = strings(t.get("effects")).unwrap_or_default();
    s.ok_codes = ints(t.get("ok_codes"));
    Ok(s)
}

fn string(v: Option<&toml::Value>) -> Option<&str> {
    v?.as_str()
}
fn strings(v: Option<&toml::Value>) -> Option<Vec<String>> {
    Some(
        v?.as_array()?
            .iter()
            .map(|x| x.as_str().map(str::to_owned))
            .collect::<Option<_>>()?,
    )
}
fn ints(v: Option<&toml::Value>) -> Option<Vec<i32>> {
    Some(
        v?.as_array()?
            .iter()
            .map(|x| x.as_integer().and_then(|n| i32::try_from(n).ok()))
            .collect::<Option<_>>()?,
    )
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

fn parse_z_records(bytes: &[u8], hint: Option<&str>) -> Option<Value> {
    let fields = hint_schema(hint);
    if fields.is_empty() {
        return None;
    }
    let mut cells = bytes.split(|b| *b == 0).collect::<Vec<_>>();
    if cells.last().is_some_and(|x| x.is_empty()) {
        cells.pop();
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

fn parse_porcelain_v2(bytes: &[u8]) -> Option<Value> {
    let mut rows = Vec::new();
    for line in text(bytes)?.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut r = Record::new();
        match line.as_bytes()[0] {
            b'?' | b'!' => {
                r.insert("status".into(), Value::Str(line[..1].into()));
                r.insert("path".into(), Value::Path(line[2..].into()));
            }
            b'1' | b'2' => {
                let parts: Vec<&str> = line.splitn(10, ' ').collect();
                if parts.len() < 9 {
                    return None;
                }
                r.insert("status".into(), Value::Str(parts[1].into()));
                let path = parts.last()?;
                r.insert("path".into(), Value::Path((*path).into()));
            }
            _ => continue,
        }
        rows.push(r);
    }
    Some(Value::Table(rows))
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
}
