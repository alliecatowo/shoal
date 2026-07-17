use super::*;

pub(super) fn parse_lines(bytes: &[u8]) -> Option<Value> {
    let mut budget = ParseBudget::default();
    let mut values = Vec::new();
    for line in text(bytes)?.lines() {
        let line = line.trim_end_matches('\r');
        if line.len() > MAX_PARSE_CELL_BYTES {
            return None;
        }
        budget.row()?;
        budget.cell(0, line.len())?;
        values.push(Value::Str(line.into()));
    }
    Some(Value::List(values))
}

pub(super) fn parse_kv(bytes: &[u8]) -> Option<Value> {
    let mut budget = ParseBudget::default();
    let mut record = Record::new();
    for line in text(bytes)?.lines().filter(|line| !line.trim().is_empty()) {
        budget.row()?;
        let (key, value) = line.split_once('=').or_else(|| line.split_once(':'))?;
        let key = key.trim();
        let value = value.trim();
        if key.is_empty()
            || key.len() > MAX_PARSE_CELL_BYTES
            || value.len() > MAX_PARSE_CELL_BYTES
            || record.contains_key(key)
        {
            return None;
        }
        budget.cell(key.len(), value.len())?;
        record.insert(key.into(), Value::Str(value.into()));
    }
    Some(Value::Record(record))
}

pub(super) fn parse_cols_n(bytes: &[u8], hint: Option<&str>, skip_lines: usize) -> Option<Value> {
    let fields = hint_schema(hint)?;
    if fields.is_empty() {
        return None;
    }
    let ((last_name, last_ty), leading_fields) = fields.split_last()?;
    let mut lines = text(bytes)?.lines();
    for _ in 0..skip_lines {
        lines.next();
    }
    let mut budget = ParseBudget::default();
    let mut rows = Vec::new();
    for line in lines.filter(|line| !line.trim().is_empty()) {
        let mut parts = line.split_whitespace();
        budget.row()?;
        let mut record = Record::new();
        for (name, ty) in leading_fields {
            let raw = parts.next()?;
            let (value, retained) = coerce_cell(raw, ty)?;
            budget.cell(name.len(), retained)?;
            record.insert(name.clone(), value);
        }
        let mut last = String::new();
        for part in parts {
            if !last.is_empty() {
                last.push(' ');
            }
            last.push_str(part);
            if last.len() > MAX_PARSE_CELL_BYTES {
                return None;
            }
        }
        if last.is_empty() {
            return None;
        }
        let (value, retained) = coerce_cell(&last, last_ty)?;
        budget.cell(last_name.len(), retained)?;
        record.insert(last_name.clone(), value);
        rows.push(record);
    }
    Some(Value::Table(rows))
}

pub(super) fn parse_tsv_headerless(bytes: &[u8], hint: Option<&str>) -> Option<Value> {
    let fields = hint_schema(hint)?;
    if fields.is_empty() {
        return None;
    }
    let mut budget = ParseBudget::default();
    let mut rows = Vec::new();
    for line in text(bytes)?.lines().filter(|line| !line.is_empty()) {
        budget.row()?;
        let mut parts = line.split('\t');
        let mut record = Record::new();
        for (name, ty) in &fields {
            let raw = parts.next()?;
            let (value, retained) = coerce_cell(raw, ty)?;
            budget.cell(name.len(), retained)?;
            record.insert(name.clone(), value);
        }
        if parts.next().is_some() {
            return None;
        }
        rows.push(record);
    }
    Some(Value::Table(rows))
}

pub(super) fn parse_z_records(bytes: &[u8], hint: Option<&str>) -> Option<Value> {
    let fields = hint_schema(hint)?;
    if fields.is_empty() {
        return None;
    }
    if bytes.is_empty() {
        return Some(Value::Table(Vec::new()));
    }
    let cell_count = bytes
        .iter()
        .filter(|byte| **byte == 0)
        .count()
        .checked_add(1)?;
    if cell_count > MAX_PARSE_CELLS {
        return None;
    }
    if bytes.iter().all(|byte| *byte == 0) {
        return Some(Value::Table(Vec::new()));
    }
    // Strip exactly one record terminator only when it is structurally extra;
    // this preserves a legitimately empty final field.
    let content = if cell_count % fields.len() == 1 && bytes.last() == Some(&0) {
        bytes.strip_suffix(&[0])?
    } else {
        bytes
    };
    let effective_cells = content
        .iter()
        .filter(|byte| **byte == 0)
        .count()
        .checked_add(1)?;
    if effective_cells % fields.len() != 0 {
        return None;
    }
    let mut budget = ParseBudget::default();
    let mut cells = content.split(|byte| *byte == 0);
    let mut rows = Vec::new();
    for _ in 0..effective_cells / fields.len() {
        budget.row()?;
        let mut record = Record::new();
        for (name, ty) in &fields {
            let raw = text(cells.next()?)?;
            let (value, retained) = coerce_cell(raw, ty)?;
            budget.cell(name.len(), retained)?;
            record.insert(name.clone(), value);
        }
        rows.push(record);
    }
    if cells.next().is_some() {
        return None;
    }
    Some(Value::Table(rows))
}

pub(super) fn parse_porcelain_v2(bytes: &[u8]) -> Option<Value> {
    let mut budget = ParseBudget::default();
    let mut rows = Vec::new();
    for line in text(bytes)?.lines() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.len() > MAX_PARSE_CELL_BYTES {
            return None;
        }
        let raw = line.as_bytes();
        let mut record = Record::new();
        budget.row()?;
        match raw.first().copied()? {
            b'?' | b'!' => {
                if raw.get(1) != Some(&b' ') || line.len() < 3 {
                    return None;
                }
                let state = if raw.first() == Some(&b'?') {
                    "untracked"
                } else {
                    "ignored"
                };
                insert_cell(&mut record, &mut budget, "status", &line[..1], "str")?;
                insert_cell(&mut record, &mut budget, "state", state, "str")?;
                insert_cell(&mut record, &mut budget, "path", &line[2..], "path")?;
            }
            b'1' => {
                let parts: Vec<&str> = line.splitn(9, ' ').collect();
                if parts.len() != 9 || parts.first() != Some(&"1") {
                    return None;
                }
                let status = *parts.get(1)?;
                if !valid_xy(status) {
                    return None;
                }
                insert_cell(&mut record, &mut budget, "status", status, "str")?;
                insert_cell(&mut record, &mut budget, "state", xy_state(status), "str")?;
                insert_cell(&mut record, &mut budget, "path", parts.get(8)?, "path")?;
            }
            b'2' => {
                let parts: Vec<&str> = line.splitn(10, ' ').collect();
                if parts.len() != 10 || parts.first() != Some(&"2") {
                    return None;
                }
                let status = *parts.get(1)?;
                if !valid_xy(status) {
                    return None;
                }
                insert_cell(&mut record, &mut budget, "status", status, "str")?;
                insert_cell(&mut record, &mut budget, "state", xy_state(status), "str")?;
                let final_field = *parts.get(9)?;
                let (path, original) = final_field
                    .split_once('\t')
                    .map_or((final_field, None), |(path, original)| {
                        (path, Some(original))
                    });
                insert_cell(&mut record, &mut budget, "path", path, "path")?;
                if let Some(original) = original {
                    insert_cell(&mut record, &mut budget, "orig", original, "path")?;
                }
            }
            _ => return None,
        }
        rows.push(record);
    }
    Some(Value::Table(rows))
}

fn insert_cell(
    record: &mut Record,
    budget: &mut ParseBudget,
    name: &str,
    raw: &str,
    ty: &str,
) -> Option<()> {
    let (value, retained) = coerce_cell(raw, ty)?;
    budget.cell(name.len(), retained)?;
    record.insert(name.into(), value);
    Some(())
}

fn valid_xy(value: &str) -> bool {
    match value.as_bytes() {
        [first, second] => {
            [first, second]
                .iter()
                .all(|byte| matches!(byte, b'.' | b'M' | b'A' | b'D' | b'R' | b'C' | b'T' | b'U'))
                && value != ".."
        }
        // Historical adapter fixtures place the similarity score in this
        // slot. Validate that legacy form exactly and map it honestly below.
        [kind @ (b'R' | b'C'), first, second, third] => {
            let _ = kind;
            [first, second, third]
                .iter()
                .all(|byte| byte.is_ascii_digit())
        }
        _ => false,
    }
}

fn xy_state(xy: &str) -> &'static str {
    fn word(character: u8) -> &'static str {
        match character {
            b'M' => "modified",
            b'A' => "added",
            b'D' => "deleted",
            b'R' => "renamed",
            b'C' => "copied",
            b'T' => "typechange",
            b'U' => "unmerged",
            _ => "unmodified",
        }
    }
    match xy.as_bytes() {
        [first, second] if *second != b'.' => word(*second),
        [first, _] => word(*first),
        [kind, _, _, _] => word(*kind),
        _ => "unmodified",
    }
}
