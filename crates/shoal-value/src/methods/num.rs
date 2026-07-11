//! Numeric-receiver unary methods (`.abs`/`.round`/`.floor`/`.ceil`).

use super::*;

pub(crate) fn numeric_unary(
    v: Value,
    ff: fn(f64) -> f64,
    fi: fn(i64) -> Option<i64>,
) -> VResult<Value> {
    match v {
        Value::Int(i) => fi(i)
            .map(Value::Int)
            .ok_or_else(|| ErrorVal::new("custom", "integer overflow")),
        Value::Float(f) => Ok(Value::Float(ff(f))),
        v => Err(ErrorVal::type_error(format!(
            "expected number, found {}",
            v.type_name()
        ))),
    }
}
pub(crate) fn float_unary(v: Value, f: fn(f64) -> f64) -> VResult<Value> {
    match v {
        Value::Float(x) => Ok(Value::Float(f(x))),
        Value::Int(i) => Ok(Value::Int(i)),
        v => Err(ErrorVal::type_error(format!(
            "expected number, found {}",
            v.type_name()
        ))),
    }
}
