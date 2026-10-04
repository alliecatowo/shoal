use super::*;

pub(super) fn parse_json(bytes: &[u8]) -> Option<Value> {
    lexically_bounded(bytes)?;
    let json = serde_json::from_slice(bytes).ok()?;
    let mut budget = ParseBudget::default();
    to_value(&json, 1, &mut budget)
}

/// Reject depth, token, and giant-string attacks before serde builds a tree.
/// The subsequent typed walk performs exact aggregate accounting.
fn lexically_bounded(bytes: &[u8]) -> Option<()> {
    let mut depth = 0usize;
    let mut nodes = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'{' | b'[' => {
                depth = depth.checked_add(1)?;
                nodes = nodes.checked_add(1)?;
                if depth > MAX_PARSE_JSON_DEPTH || nodes > MAX_PARSE_JSON_NODES {
                    return None;
                }
                index += 1;
            }
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                index += 1;
            }
            b'"' => {
                nodes = nodes.checked_add(1)?;
                if nodes > MAX_PARSE_JSON_NODES {
                    return None;
                }
                index += 1;
                let start = index;
                let mut escaped = false;
                while index < bytes.len() {
                    let byte = bytes[index];
                    if !escaped && byte == b'"' {
                        break;
                    }
                    escaped = !escaped && byte == b'\\';
                    if byte != b'\\' {
                        escaped = false;
                    }
                    index += 1;
                    if index.saturating_sub(start) > MAX_PARSE_CELL_BYTES {
                        return None;
                    }
                }
                if bytes.get(index) != Some(&b'"') {
                    return None;
                }
                index += 1;
            }
            byte if byte.is_ascii_whitespace() || matches!(byte, b',' | b':') => index += 1,
            _ => {
                nodes = nodes.checked_add(1)?;
                if nodes > MAX_PARSE_JSON_NODES {
                    return None;
                }
                while index < bytes.len()
                    && !bytes[index].is_ascii_whitespace()
                    && !matches!(bytes[index], b',' | b':' | b'{' | b'}' | b'[' | b']' | b'"')
                {
                    index += 1;
                }
            }
        }
    }
    (depth == 0).then_some(())
}

fn to_value(json: &serde_json::Value, depth: usize, budget: &mut ParseBudget) -> Option<Value> {
    match json {
        serde_json::Value::Null => {
            budget.json_node(depth, 0)?;
            Some(Value::Null)
        }
        serde_json::Value::Bool(value) => {
            budget.json_node(depth, 0)?;
            Some(Value::Bool(*value))
        }
        serde_json::Value::Number(value) => {
            budget.json_node(depth, 0)?;
            if let Some(value) = value.as_i64() {
                Some(Value::Int(value))
            } else if value.is_u64() {
                // Shoal has no lossless u64/bignum value. Do not silently turn
                // an external identifier above i64::MAX into a rounded float.
                None
            } else {
                let value = value.as_f64()?;
                value.is_finite().then_some(Value::Float(value))
            }
        }
        serde_json::Value::String(value) => {
            if value.len() > MAX_PARSE_CELL_BYTES {
                return None;
            }
            budget.json_node(depth, value.len())?;
            Some(Value::Str(value.clone()))
        }
        serde_json::Value::Array(values) => {
            budget.json_node(depth, 0)?;
            let mut converted = Vec::with_capacity(values.len().min(MAX_PARSE_JSON_NODES));
            for value in values {
                converted.push(to_value(value, depth + 1, budget)?);
            }
            if !converted.is_empty()
                && converted
                    .iter()
                    .all(|value| matches!(value, Value::Record(_)))
            {
                let mut rows = Vec::with_capacity(converted.len());
                for value in converted {
                    let Value::Record(row) = value else {
                        return None;
                    };
                    rows.push(row);
                }
                Some(Value::Table(rows))
            } else {
                Some(Value::List(converted))
            }
        }
        serde_json::Value::Object(values) => {
            budget.json_node(depth, std::mem::size_of::<Record>())?;
            budget.row()?;
            let mut record = Record::new();
            for (key, value) in values {
                if key.len() > MAX_PARSE_CELL_BYTES {
                    return None;
                }
                budget.cell(key.len(), 0)?;
                record.insert(key.clone(), to_value(value, depth + 1, budget)?);
            }
            Some(Value::Record(record))
        }
    }
}

pub(super) fn parse_ndjson(bytes: &[u8]) -> Option<Value> {
    let mut budget = ParseBudget::default();
    let mut values = Vec::new();
    for line in text(bytes)?.lines().filter(|line| !line.trim().is_empty()) {
        if line.len() > MAX_PARSE_CELL_BYTES {
            return None;
        }
        budget.row()?;
        lexically_bounded(line.as_bytes())?;
        let json = serde_json::from_str(line).ok()?;
        values.push(to_value(&json, 1, &mut budget)?);
    }
    rows_or_list(values)
}

fn rows_or_list(values: Vec<Value>) -> Option<Value> {
    if values.iter().all(|value| matches!(value, Value::Record(_))) {
        let mut rows = Vec::with_capacity(values.len());
        for value in values {
            let Value::Record(row) = value else {
                return None;
            };
            rows.push(row);
        }
        Some(Value::Table(rows))
    } else {
        Some(Value::List(values))
    }
}
