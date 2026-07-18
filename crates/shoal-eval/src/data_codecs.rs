//! Shared admission and bounded output for structured-data namespaces.

use serde::Serialize;
use shoal_value::{
    CallArgs, ErrorVal, OpaqueHandling, Record, RetainedLimits, VResult, Value, json_to_value,
    preflight_json_numbers, retained_size, value_to_json,
};
use std::io::{self, Write};

pub(crate) const MAX_DATA_INPUT_BYTES: usize = 16 * 1024 * 1024;
const MAX_DATA_RETAINED_BYTES: usize = 16 * 1024 * 1024;
const MAX_DATA_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_DATA_NODES: usize = 131_072;
const MAX_DATA_DEPTH: usize = 128;
const MAX_CSV_ROWS: usize = 16_384;
const MAX_CSV_CELLS: usize = 131_072;

pub(crate) fn call(namespace: &str, method: &str, args: CallArgs) -> VResult<Value> {
    match (namespace, method) {
        ("json", "parse") => parse_json(&args),
        ("json", "stringify") => stringify_json(&args),
        ("yaml", "parse") => parse_yaml(&args),
        ("yaml", "stringify") => stringify_yaml(&args),
        ("toml", "parse") => parse_toml(&args),
        ("toml", "stringify") => stringify_toml(&args),
        ("csv", "parse") => parse_csv(&args),
        ("csv", "stringify") => stringify_csv(&args),
        _ => Err(ErrorVal::new(
            "field_missing",
            format!("unknown method `{namespace}.{method}`"),
        )),
    }
}

pub(crate) fn http_json_projection(source: &str) -> Value {
    parse_json_text(source, "http response JSON").unwrap_or(Value::Null)
}

fn data_limit(what: &str, detail: impl std::fmt::Display) -> ErrorVal {
    ErrorVal::new("data_materialization_limit", format!("{what}: {detail}"))
        .with_hint("use a stream or split the document into bounded records")
}

fn source_str<'a>(args: &'a CallArgs, what: &str) -> VResult<&'a str> {
    let source = match args.pos.first() {
        Some(Value::Str(source)) => source,
        Some(value) => {
            return Err(ErrorVal::arg_error(format!(
                "{what} expects a str, found {}",
                value.type_name()
            )));
        }
        None => {
            return Err(ErrorVal::arg_error(format!(
                "{what} expects a str argument"
            )));
        }
    };
    if source.len() > MAX_DATA_INPUT_BYTES {
        return Err(data_limit(
            what,
            format_args!(
                "input is {} bytes; the limit is {MAX_DATA_INPUT_BYTES}",
                source.len()
            ),
        ));
    }
    Ok(source)
}

fn source_value<'a>(args: &'a CallArgs, what: &str) -> VResult<&'a Value> {
    let value = args
        .pos
        .first()
        .ok_or_else(|| ErrorVal::arg_error(format!("{what} expects a value")))?;
    admit_source_value(value, what)?;
    Ok(value)
}

fn parse_json(args: &CallArgs) -> VResult<Value> {
    parse_json_text(source_str(args, "json.parse")?, "json.parse")
}

fn parse_json_text(source: &str, what: &str) -> VResult<Value> {
    if source.len() > MAX_DATA_INPUT_BYTES {
        return Err(data_limit(
            what,
            format_args!(
                "input is {} bytes; the limit is {MAX_DATA_INPUT_BYTES}",
                source.len()
            ),
        ));
    }
    let parsed: serde_json::Value = serde_json::from_str(source)
        .map_err(|error| ErrorVal::arg_error(format!("{what}: {error}")))?;
    preflight_json_numbers(source, what)?;
    checked_json_to_value(&parsed, what)
}

fn parse_yaml(args: &CallArgs) -> VResult<Value> {
    let source = source_str(args, "yaml.parse")?;
    let parsed: serde_json::Value = serde_norway::from_str(source)
        .map_err(|error| ErrorVal::arg_error(format!("yaml.parse: {error}")))?;
    checked_json_to_value(&parsed, "yaml.parse")
}

fn parse_toml(args: &CallArgs) -> VResult<Value> {
    let source = source_str(args, "toml.parse")?;
    let parsed: serde_json::Value = toml::from_str(source)
        .map_err(|error| ErrorVal::arg_error(format!("toml.parse: {error}")))?;
    checked_json_to_value(&parsed, "toml.parse")
}

fn checked_json_to_value(parsed: &serde_json::Value, what: &str) -> VResult<Value> {
    admit_json_tree(parsed, what)?;
    let value = json_to_value(parsed).map_err(|mut error| {
        error.msg = format!("{what}: {}", error.msg);
        error
    })?;
    admit_retained_value(&value, what, OpaqueHandling::Reject, false)?;
    Ok(value)
}

fn admit_json_tree(root: &serde_json::Value, what: &str) -> VResult<()> {
    let mut stack = vec![(root, 1usize)];
    let mut nodes = 0usize;
    while let Some((value, depth)) = stack.pop() {
        if depth > MAX_DATA_DEPTH {
            return Err(data_limit(
                what,
                format_args!("document depth exceeds {MAX_DATA_DEPTH}"),
            ));
        }
        nodes = nodes
            .checked_add(1)
            .ok_or_else(|| data_limit(what, "node accounting overflowed"))?;
        if nodes > MAX_DATA_NODES {
            return Err(data_limit(
                what,
                format_args!("document has more than {MAX_DATA_NODES} nodes"),
            ));
        }
        match value {
            serde_json::Value::Array(values) => {
                stack.extend(values.iter().map(|value| (value, depth + 1)));
            }
            serde_json::Value::Object(fields) => {
                nodes = nodes
                    .checked_add(fields.len())
                    .ok_or_else(|| data_limit(what, "node accounting overflowed"))?;
                if nodes > MAX_DATA_NODES {
                    return Err(data_limit(
                        what,
                        format_args!("document has more than {MAX_DATA_NODES} nodes"),
                    ));
                }
                stack.extend(fields.values().map(|value| (value, depth + 1)));
            }
            _ => {}
        }
    }
    Ok(())
}

fn admit_retained_value(
    value: &Value,
    what: &str,
    opaque: OpaqueHandling,
    allow_secret: bool,
) -> VResult<()> {
    measure_retained_value(value, what, MAX_DATA_RETAINED_BYTES, opaque, allow_secret).map(|_| ())
}

fn measure_retained_value(
    value: &Value,
    what: &str,
    max_bytes: usize,
    opaque: OpaqueHandling,
    allow_secret: bool,
) -> VResult<usize> {
    retained_size(
        value,
        RetainedLimits {
            max_bytes,
            max_depth: MAX_DATA_DEPTH,
            max_nodes: MAX_DATA_NODES,
            opaque,
            allow_secret,
        },
    )
    .map_err(|error| data_limit(what, format_args!("value admission failed: {error:?}")))
}

fn admit_source_value(value: &Value, what: &str) -> VResult<()> {
    admit_retained_value(value, what, OpaqueHandling::Charge(1024), true)?;
    let mut stack = vec![value];
    let mut logical_nodes = 0usize;
    while let Some(value) = stack.pop() {
        logical_nodes = logical_nodes
            .checked_add(1)
            .ok_or_else(|| data_limit(what, "node accounting overflowed"))?;
        match value {
            Value::List(values) => stack.extend(values),
            Value::Record(record) => stack.extend(record.values()),
            Value::Table(rows) => {
                for row in rows {
                    logical_nodes = logical_nodes
                        .checked_add(1)
                        .ok_or_else(|| data_limit(what, "node accounting overflowed"))?;
                    stack.extend(row.values());
                }
            }
            Value::Range(range) => {
                logical_nodes = logical_nodes
                    .checked_add(range.materialization_len()?)
                    .ok_or_else(|| data_limit(what, "range node accounting overflowed"))?;
            }
            _ => {}
        }
        if logical_nodes > MAX_DATA_NODES {
            return Err(data_limit(
                what,
                format_args!("value would encode more than {MAX_DATA_NODES} nodes"),
            ));
        }
    }
    Ok(())
}

fn stringify_json(args: &CallArgs) -> VResult<Value> {
    let value = source_value(args, "json.stringify")?;
    let pretty = matches!(args.get_named("pretty"), Some(Value::Bool(true)))
        || matches!(args.pos.get(1), Some(Value::Bool(true)));
    let parsed = value_to_json(value)?;
    let mut output = LimitedWriter::new(MAX_DATA_OUTPUT_BYTES);
    let result = if pretty {
        serde_json::to_writer_pretty(&mut output, &parsed)
    } else {
        serde_json::to_writer(&mut output, &parsed)
    };
    finish_output(output, result, "json.stringify")
}

fn stringify_yaml(args: &CallArgs) -> VResult<Value> {
    let value = source_value(args, "yaml.stringify")?;
    let parsed = value_to_json(value)?;
    let mut output = LimitedWriter::new(MAX_DATA_OUTPUT_BYTES);
    let result = serde_norway::to_writer(&mut output, &parsed);
    finish_output(output, result, "yaml.stringify")
}

fn stringify_toml(args: &CallArgs) -> VResult<Value> {
    let value = source_value(args, "toml.stringify")?;
    let parsed = value_to_json(value)?;
    admit_toml_output_bound(&parsed)?;

    let mut buffer = toml::ser::Buffer::new();
    parsed
        .serialize(toml::Serializer::new(&mut buffer))
        .map_err(|error| {
            ErrorVal::new(
                "arg_error",
                format!("toml.stringify: {error} (toml needs a record/table at the top level)"),
            )
        })?;
    let mut output = LimitedWriter::new(MAX_DATA_OUTPUT_BYTES);
    let result = write!(&mut output, "{buffer}");
    finish_output(output, result, "toml.stringify")
}

/// TOML's serializer owns per-table strings instead of accepting a writer.
/// Charge a conservative upper bound before it allocates: every scalar/key
/// byte may escape sixfold, every node receives syntax slack, and every node
/// is charged for its full ancestor key path (table headers repeat paths).
fn admit_toml_output_bound(root: &serde_json::Value) -> VResult<()> {
    let mut stack = vec![(root, 0usize)];
    let mut estimate = 0usize;
    while let Some((value, path_bytes)) = stack.pop() {
        add_output_estimate(&mut estimate, path_bytes.saturating_add(64))?;
        match value {
            serde_json::Value::String(text) => {
                add_output_estimate(&mut estimate, text.len().saturating_mul(6))?;
            }
            serde_json::Value::Array(values) => {
                stack.extend(values.iter().map(|value| (value, path_bytes)));
            }
            serde_json::Value::Object(fields) => {
                for (key, value) in fields {
                    let key_bytes = key.len().saturating_mul(6).saturating_add(4);
                    add_output_estimate(&mut estimate, key_bytes)?;
                    stack.push((value, path_bytes.saturating_add(key_bytes)));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn add_output_estimate(estimate: &mut usize, amount: usize) -> VResult<()> {
    *estimate = estimate
        .checked_add(amount)
        .ok_or_else(|| data_limit("toml.stringify", "output-size accounting overflowed"))?;
    if *estimate > MAX_DATA_OUTPUT_BYTES {
        return Err(data_limit(
            "toml.stringify",
            format_args!("encoded output could exceed {MAX_DATA_OUTPUT_BYTES} bytes"),
        ));
    }
    Ok(())
}

fn parse_csv(args: &CallArgs) -> VResult<Value> {
    let source = source_str(args, "csv.parse")?;
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(true)
        .from_reader(source.as_bytes());
    let headers = reader
        .headers()
        .map_err(|error| ErrorVal::arg_error(format!("csv.parse: {error}")))?
        .clone();
    let mut cells = headers.len();
    if cells > MAX_CSV_CELLS {
        return Err(data_limit(
            "csv.parse",
            format_args!("header exceeds the {MAX_CSV_CELLS}-cell limit"),
        ));
    }
    let mut rows = Vec::new();
    let mut retained_rows = 0usize;
    for record in reader.records() {
        if rows.len() >= MAX_CSV_ROWS {
            return Err(data_limit(
                "csv.parse",
                format_args!("table exceeds the {MAX_CSV_ROWS}-row limit"),
            ));
        }
        let record = record.map_err(|error| ErrorVal::arg_error(format!("csv.parse: {error}")))?;
        cells = cells
            .checked_add(record.len())
            .ok_or_else(|| data_limit("csv.parse", "cell accounting overflowed"))?;
        if cells > MAX_CSV_CELLS {
            return Err(data_limit(
                "csv.parse",
                format_args!("table exceeds the {MAX_CSV_CELLS}-cell limit"),
            ));
        }
        let mut row = Record::new();
        for (header, field) in headers.iter().zip(record.iter()) {
            row.insert(header.to_string(), Value::Str(field.to_string()));
        }
        let row = Value::Record(row);
        let retained = measure_retained_value(
            &row,
            "csv.parse",
            MAX_DATA_RETAINED_BYTES.saturating_sub(retained_rows),
            OpaqueHandling::Reject,
            false,
        )?;
        retained_rows = retained_rows
            .checked_add(retained)
            .ok_or_else(|| data_limit("csv.parse", "retained-value accounting overflowed"))?;
        let Value::Record(row) = row else {
            unreachable!("the admitted CSV row was just constructed as a record")
        };
        rows.push(row);
    }
    let value = Value::Table(rows);
    admit_retained_value(&value, "csv.parse", OpaqueHandling::Reject, false)?;
    Ok(value)
}

fn stringify_csv(args: &CallArgs) -> VResult<Value> {
    let value = source_value(args, "csv.stringify")?;
    let rows: Vec<&Record> = match value {
        Value::Table(rows) => rows.iter().collect(),
        Value::List(values) => values
            .iter()
            .map(|value| match value {
                Value::Record(record) => Ok(record),
                other => Err(ErrorVal::type_error(format!(
                    "csv.stringify expects a list of records, found a {}",
                    other.type_name()
                ))),
            })
            .collect::<VResult<_>>()?,
        Value::Record(record) => vec![record],
        other => {
            return Err(ErrorVal::type_error(format!(
                "csv.stringify expects a table, found {}",
                other.type_name()
            )));
        }
    };
    if rows.len() > MAX_CSV_ROWS {
        return Err(data_limit(
            "csv.stringify",
            format_args!("table exceeds the {MAX_CSV_ROWS}-row limit"),
        ));
    }
    let columns = rows.first().map_or(0, |row| row.len());
    if rows.len().saturating_mul(columns) > MAX_CSV_CELLS {
        return Err(data_limit(
            "csv.stringify",
            format_args!("table exceeds the {MAX_CSV_CELLS}-cell limit"),
        ));
    }

    let mut output = LimitedWriter::new(MAX_DATA_OUTPUT_BYTES);
    let result = (|| -> Result<(), csv::Error> {
        let mut writer = csv::Writer::from_writer(&mut output);
        if let Some(first) = rows.first() {
            writer.write_record(first.keys().map(String::as_str))?;
            for row in &rows {
                writer.write_record(first.keys().map(|key| match row.get(key) {
                    Some(Value::Str(text)) => text.clone(),
                    Some(other) => shoal_value::render::render_inline(other),
                    None => String::new(),
                }))?;
            }
        }
        writer.flush()?;
        Ok(())
    })();
    finish_output(output, result, "csv.stringify")
}

struct LimitedWriter {
    bytes: Vec<u8>,
    max_bytes: usize,
    exceeded: bool,
}

impl LimitedWriter {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(max_bytes.min(8 * 1024)),
            max_bytes,
            exceeded: false,
        }
    }
}

impl Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.max_bytes.saturating_sub(self.bytes.len()) {
            self.exceeded = true;
            return Err(io::Error::other("structured-data output limit exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn finish_output<E: std::fmt::Display>(
    output: LimitedWriter,
    result: Result<(), E>,
    what: &str,
) -> VResult<Value> {
    if output.exceeded {
        return Err(data_limit(
            what,
            format_args!("encoded output exceeds {MAX_DATA_OUTPUT_BYTES} bytes"),
        ));
    }
    result.map_err(|error| ErrorVal::new("custom", format!("{what}: {error}")))?;
    String::from_utf8(output.bytes)
        .map(Value::Str)
        .map_err(|_| ErrorVal::new("utf8_error", format!("{what} produced non-UTF-8")))
}

#[cfg(test)]
mod tests;
