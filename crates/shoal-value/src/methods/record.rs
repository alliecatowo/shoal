//! Record-only methods (`.keys`/`.values`/`.items`/`.set`/`.merge`).

use super::*;

/// `.set(key, value)` — a new record with `key` inserted or replaced. Records
/// are values: the receiver is unchanged, and an existing key keeps its
/// position (only its value changes).
pub(crate) fn set(v: Value, key: &str, value: Value) -> VResult<Value> {
    match v {
        Value::Record(mut r) => {
            r.insert(key.to_string(), value);
            Ok(Value::Record(r))
        }
        v => Err(ErrorVal::type_error(format!(
            "expected record, found {}",
            v.type_name()
        ))),
    }
}

/// `.merge(other)` — a new record with `other`'s keys layered over the
/// receiver's (right wins on collision; existing keys keep their position, new
/// keys append). Both operands stay unchanged.
pub(crate) fn merge(v: Value, other: Value) -> VResult<Value> {
    match (v, other) {
        (Value::Record(mut r), Value::Record(o)) => {
            for (k, val) in o {
                r.insert(k, val);
            }
            Ok(Value::Record(r))
        }
        (Value::Record(_), other) => Err(ErrorVal::type_error(format!(
            "merge expects a record argument, found {}",
            other.type_name()
        ))),
        (v, _) => Err(ErrorVal::type_error(format!(
            "expected record, found {}",
            v.type_name()
        ))),
    }
}

pub(crate) fn record_side(v: Value, keys: bool) -> VResult<Value> {
    match v {
        Value::Record(r) => Ok(Value::List(if keys {
            r.keys().cloned().map(Value::Str).collect()
        } else {
            r.into_values().collect()
        })),
        v => Err(ErrorVal::type_error(format!(
            "expected record, found {}",
            v.type_name()
        ))),
    }
}
pub(crate) fn items(v: Value) -> VResult<Value> {
    match v {
        Value::Record(r) => Ok(Value::List(
            r.into_iter()
                .map(|(k, v)| Value::List(vec![Value::Str(k), v]))
                .collect(),
        )),
        v => Err(ErrorVal::type_error(format!(
            "expected record, found {}",
            v.type_name()
        ))),
    }
}
