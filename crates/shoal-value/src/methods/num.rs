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
/// `.round(n)`/`.floor(n)`/`.ceil(n)` — apply the rounding op at `n` decimal
/// places (`n` defaults to 0 → nearest integer). Ints pass through unchanged at
/// any precision. Previously the argument was silently ignored, so `.round(2)`
/// returned the integer-rounded value.
pub(crate) fn round_to(v: Value, ndigits: usize, f: fn(f64) -> f64) -> VResult<Value> {
    match v {
        Value::Int(i) => Ok(Value::Int(i)),
        Value::Float(x) => {
            let factor = 10f64.powi(ndigits.min(i32::MAX as usize) as i32);
            Ok(Value::Float(f(x * factor) / factor))
        }
        v => Err(ErrorVal::type_error(format!(
            "expected number, found {}",
            v.type_name()
        ))),
    }
}
