//! Record-only methods (`.keys`/`.values`/`.items`).

use super::*;

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
