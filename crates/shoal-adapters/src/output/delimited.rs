use std::collections::BTreeSet;

use super::*;

pub(super) fn parse(bytes: &[u8], delimiter: u8, hint: Option<&str>) -> Option<Value> {
    let schema = hint_schema(hint)?;
    let mut budget = ParseBudget::default();
    let mut header: Option<Vec<String>> = None;
    let mut types = Vec::new();
    let mut rows = Vec::new();
    visit_rows(bytes, delimiter, |cells| {
        if header.is_none() {
            if cells.is_empty()
                || cells.len() > MAX_PARSE_COLUMNS
                || cells.iter().any(String::is_empty)
                || !all_unique(cells.iter().map(String::as_str))
            {
                return None;
            }
            types = cells
                .iter()
                .map(|name| {
                    schema
                        .iter()
                        .find(|(schema_name, _)| schema_name == name)
                        .map_or("str", |(_, ty)| ty.as_str())
                        .to_owned()
                })
                .collect();
            header = Some(cells);
            return Some(());
        }
        let names = header.as_ref()?;
        if cells.len() != names.len() {
            return None;
        }
        budget.row()?;
        let mut record = Record::new();
        for ((name, ty), raw) in names.iter().zip(&types).zip(&cells) {
            let (value, retained) = coerce_cell(raw, ty)?;
            budget.cell(name.len(), retained)?;
            record.insert(name.clone(), value);
        }
        rows.push(record);
        Some(())
    })?;
    header?;
    Some(Value::Table(rows))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CsvState {
    FieldStart,
    Unquoted,
    Quoted,
    QuoteClosed,
}

/// Single-row buffering avoids duplicating the complete input before records
/// are constructed.
fn visit_rows(
    bytes: &[u8],
    delimiter: u8,
    mut visit: impl FnMut(Vec<String>) -> Option<()>,
) -> Option<()> {
    let source = text(bytes)?;
    let mut row = Vec::new();
    let mut field = String::new();
    let mut state = CsvState::FieldStart;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        match state {
            CsvState::FieldStart => match byte {
                b'"' => state = CsvState::Quoted,
                b'\n' => finish_row(&mut row, &mut field, &mut visit)?,
                b'\r' if bytes.get(index + 1) == Some(&b'\n') => {}
                b'\r' => return None,
                value if value == delimiter => push_field(&mut row, &mut field)?,
                _ => {
                    push_source_char(source, &mut index, &mut field)?;
                    state = CsvState::Unquoted;
                }
            },
            CsvState::Unquoted => match byte {
                b'"' => return None,
                b'\n' => {
                    finish_row(&mut row, &mut field, &mut visit)?;
                    state = CsvState::FieldStart;
                }
                b'\r' if bytes.get(index + 1) == Some(&b'\n') => {}
                b'\r' => return None,
                value if value == delimiter => {
                    push_field(&mut row, &mut field)?;
                    state = CsvState::FieldStart;
                }
                _ => push_source_char(source, &mut index, &mut field)?,
            },
            CsvState::Quoted => match byte {
                b'"' => state = CsvState::QuoteClosed,
                _ => push_source_char(source, &mut index, &mut field)?,
            },
            CsvState::QuoteClosed => match byte {
                b'"' => {
                    field.push('"');
                    state = CsvState::Quoted;
                }
                b'\n' => {
                    finish_row(&mut row, &mut field, &mut visit)?;
                    state = CsvState::FieldStart;
                }
                b'\r' if bytes.get(index + 1) == Some(&b'\n') => {}
                value if value == delimiter => {
                    push_field(&mut row, &mut field)?;
                    state = CsvState::FieldStart;
                }
                _ => return None,
            },
        }
        if field.len() > MAX_PARSE_CELL_BYTES {
            return None;
        }
        index += 1;
    }
    if state == CsvState::Quoted {
        return None;
    }
    if state != CsvState::FieldStart || !row.is_empty() || !field.is_empty() {
        finish_row(&mut row, &mut field, &mut visit)?;
    }
    Some(())
}

fn push_source_char(source: &str, index: &mut usize, field: &mut String) -> Option<()> {
    let character = source.get(*index..)?.chars().next()?;
    field.push(character);
    *index = index.checked_add(character.len_utf8().saturating_sub(1))?;
    Some(())
}

fn push_field(row: &mut Vec<String>, field: &mut String) -> Option<()> {
    if row.len() >= MAX_PARSE_COLUMNS {
        return None;
    }
    row.push(std::mem::take(field));
    Some(())
}

fn finish_row(
    row: &mut Vec<String>,
    field: &mut String,
    visit: &mut impl FnMut(Vec<String>) -> Option<()>,
) -> Option<()> {
    push_field(row, field)?;
    visit(std::mem::take(row))
}

fn all_unique<'a>(values: impl Iterator<Item = &'a str>) -> bool {
    let mut seen = BTreeSet::new();
    values.into_iter().all(|value| seen.insert(value))
}
