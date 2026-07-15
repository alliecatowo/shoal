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
            .ok_or_else(|| ErrorVal::new("overflow", "integer overflow")),
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
///
/// A large `n` (or a large-magnitude `x`) makes the `10^n` scale factor — or
/// the scaled value `x * factor` — overflow f64 to ±inf, which the old code
/// then divided back into `nan`/`inf`, silently corrupting a finite input.
/// But rounding a finite f64 to more decimal places than it can represent (or
/// rounding a value already far larger than its fractional resolution) is the
/// IDENTITY: the value has no representable fractional detail to drop. So when
/// scaling overflows we return `x` unchanged rather than a non-finite result —
/// keeping the output finite and mathematically correct (`1.23456.round(400)`
/// is `1.23456`), with no need to reject an otherwise well-defined precision.
pub(crate) fn round_to(v: Value, ndigits: usize, f: fn(f64) -> f64) -> VResult<Value> {
    match v {
        Value::Int(i) => Ok(Value::Int(i)),
        Value::Float(x) => {
            let factor = 10f64.powi(ndigits.min(i32::MAX as usize) as i32);
            let scaled = x * factor;
            if scaled.is_finite() {
                Ok(Value::Float(f(scaled) / factor))
            } else {
                // `factor` or `x * factor` overflowed (or `x` was already
                // non-finite): rounding leaves `x` unchanged.
                Ok(Value::Float(x))
            }
        }
        v => Err(ErrorVal::type_error(format!(
            "expected number, found {}",
            v.type_name()
        ))),
    }
}
