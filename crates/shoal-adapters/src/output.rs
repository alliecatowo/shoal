//! Structured-output decoders for declarative command adapters.
//!
//! Adapter output is untrusted process output. Every parser shares these byte,
//! shape, and retained-value ceilings. `None` is the public mismatch signal:
//! callers retain the original bytes instead of presenting a partial value.

use shoal_value::{Record, Value};

mod delimited;
mod json;
mod records;

pub const MAX_PARSE_INPUT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PARSE_HINT_BYTES: usize = 64 * 1024;
pub const MAX_PARSE_ROWS: usize = 65_536;
pub const MAX_PARSE_COLUMNS: usize = 256;
pub const MAX_PARSE_CELLS: usize = 262_144;
pub const MAX_PARSE_CELL_BYTES: usize = 1024 * 1024;
pub const MAX_PARSE_RETAINED_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_PARSE_JSON_DEPTH: usize = 64;
pub const MAX_PARSE_JSON_NODES: usize = 262_144;

const VALUE_OVERHEAD_BYTES: usize = 64;

#[derive(Default)]
struct ParseBudget {
    rows: usize,
    cells: usize,
    json_nodes: usize,
    retained_bytes: usize,
}

impl ParseBudget {
    fn retain(&mut self, bytes: usize) -> Option<()> {
        self.retained_bytes = self.retained_bytes.checked_add(bytes)?;
        (self.retained_bytes <= MAX_PARSE_RETAINED_BYTES).then_some(())
    }

    fn row(&mut self) -> Option<()> {
        self.rows = self.rows.checked_add(1)?;
        if self.rows > MAX_PARSE_ROWS {
            return None;
        }
        self.retain(std::mem::size_of::<Record>())
    }

    fn cell(&mut self, key_bytes: usize, value_bytes: usize) -> Option<()> {
        self.cells = self.cells.checked_add(1)?;
        if self.cells > MAX_PARSE_CELLS || key_bytes > MAX_PARSE_CELL_BYTES {
            return None;
        }
        self.retain(
            key_bytes
                .checked_add(value_bytes)?
                .checked_add(VALUE_OVERHEAD_BYTES)?,
        )
    }

    fn json_node(&mut self, depth: usize, retained_bytes: usize) -> Option<()> {
        if depth > MAX_PARSE_JSON_DEPTH {
            return None;
        }
        self.json_nodes = self.json_nodes.checked_add(1)?;
        if self.json_nodes > MAX_PARSE_JSON_NODES {
            return None;
        }
        self.retain(retained_bytes.checked_add(VALUE_OVERHEAD_BYTES)?)
    }
}

/// Parse bounded, complete process output into a structured value.
///
/// `None` means the strategy did not match or a resource/shape ceiling was
/// reached. No parser returns a structured prefix.
pub fn parse_output(strategy: &str, bytes: &[u8], type_hint: Option<&str>) -> Option<Value> {
    if strategy == "none" {
        return None;
    }
    if bytes.len() > MAX_PARSE_INPUT_BYTES
        || type_hint.is_some_and(|hint| hint.len() > MAX_PARSE_HINT_BYTES)
    {
        return None;
    }
    match strategy {
        "json" => json::parse_json(bytes),
        "ndjson" => json::parse_ndjson(bytes),
        "lines" => records::parse_lines(bytes),
        "kv" => records::parse_kv(bytes),
        "csv" => delimited::parse(bytes, b',', type_hint),
        "tsv" => delimited::parse(bytes, b'\t', type_hint),
        "z-records" => records::parse_z_records(bytes, type_hint),
        "porcelain-v2" => records::parse_porcelain_v2(bytes),
        "cols" => records::parse_cols_n(bytes, type_hint, 1),
        "cols2" => records::parse_cols_n(bytes, type_hint, 2),
        "tsv-headerless" => records::parse_tsv_headerless(bytes, type_hint),
        _ => None,
    }
}

fn text(bytes: &[u8]) -> Option<&str> {
    std::str::from_utf8(bytes).ok()
}

fn hint_schema(hint: Option<&str>) -> Option<Vec<(String, String)>> {
    let Some(hint) = hint else {
        return Some(Vec::new());
    };
    if hint.len() > MAX_PARSE_HINT_BYTES {
        return None;
    }
    let body = hint
        .split_once("<{")
        .and_then(|(_, body)| body.strip_suffix("}>"))?;
    let mut fields = Vec::new();
    for field in body.split(',') {
        let (name, ty) = field.split_once(':')?;
        let name = name.trim();
        let ty = ty.trim().trim_end_matches('?');
        if name.is_empty()
            || ty.is_empty()
            || name.len() > MAX_PARSE_CELL_BYTES
            || ty.len() > MAX_PARSE_CELL_BYTES
            || fields.len() >= MAX_PARSE_COLUMNS
            || fields.iter().any(|(existing, _)| existing == name)
        {
            return None;
        }
        fields.push((name.into(), ty.into()));
    }
    Some(fields)
}

fn coerce_cell(raw: &str, ty: &str) -> Option<(Value, usize)> {
    if raw.len() > MAX_PARSE_CELL_BYTES {
        return None;
    }
    Some(match ty {
        "str" | "datetime" => (Value::Str(raw.into()), raw.len()),
        "path" => (Value::Path(raw.into()), raw.len()),
        "int" => (Value::Int(raw.parse().ok()?), 0),
        "float" => {
            let value: f64 = raw.parse().ok()?;
            if !value.is_finite() {
                return None;
            }
            (Value::Float(value), 0)
        }
        "bool" => (Value::Bool(raw.parse().ok()?), 0),
        "size" => (Value::Size(shoal_value::parse_size(raw)?), 0),
        "size_kb" => (Value::Size(parse_size_kb(raw)?), 0),
        "duration" => (Value::Duration(shoal_value::parse_duration(raw)?), 0),
        "time" => (Value::Time(shoal_value::parse_time(raw)?), 0),
        _ => return None,
    })
}

fn parse_size_kb(raw: &str) -> Option<u64> {
    if let Ok(kilobytes) = raw.parse::<u64>() {
        return kilobytes.checked_mul(1024);
    }
    let kilobytes: f64 = raw.parse().ok()?;
    let bytes = kilobytes * 1024.0;
    if !bytes.is_finite() || bytes < 0.0 || bytes >= u64::MAX as f64 {
        return None;
    }
    Some(bytes.round() as u64)
}
