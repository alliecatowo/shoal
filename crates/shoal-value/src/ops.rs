//! Binary/unary operator semantics — the complete coercion matrix of TDD §4.2.
//!
//! Exactly two coercion sites exist in shoal; this file is site 1 (arithmetic
//! promotion). Everything not listed here is a type error. Two non-coercive
//! overloads ride along: `str + str` and `list + list` concatenation.

use crate::{ErrorVal, VResult, Value};
use shoal_ast::BinOp;
use std::cmp::Ordering;

fn type_err(op: &str, l: &Value, r: &Value) -> ErrorVal {
    ErrorVal::type_error(format!(
        "cannot apply `{op}` to {} and {}",
        l.type_name(),
        r.type_name()
    ))
}

fn int_op(op: BinOp, a: i64, b: i64) -> VResult<Value> {
    let out = match op {
        BinOp::Add => a.checked_add(b),
        BinOp::Sub => a.checked_sub(b),
        BinOp::Mul => a.checked_mul(b),
        BinOp::Div => {
            if b == 0 {
                return Err(ErrorVal::new("div_zero", "division by zero"));
            }
            a.checked_div(b)
        }
        BinOp::Rem => {
            if b == 0 {
                return Err(ErrorVal::new("div_zero", "remainder by zero"));
            }
            a.checked_rem(b)
        }
        _ => unreachable!(),
    };
    out.map(Value::Int)
        .ok_or_else(|| ErrorVal::new("overflow", format!("integer overflow in `{a} op {b}`")))
}

fn float_op(op: BinOp, a: f64, b: f64) -> Value {
    Value::Float(match op {
        BinOp::Add => a + b,
        BinOp::Sub => a - b,
        BinOp::Mul => a * b,
        BinOp::Div => a / b,
        BinOp::Rem => a % b,
        _ => unreachable!(),
    })
}

/// `size * float` (either operand order). The float→int `as` cast SATURATES
/// (NaN/negative→0, overflow→u64::MAX), which silently produced garbage sizes
/// where the integer arms (`checked_mul` + `< 0` guard) correctly error. Range-
/// check the rounded product before casting and mirror the integer arms' exact
/// error codes/messages: a negative product is `type_error: size cannot go
/// negative`; a non-finite or out-of-u64-range product is `overflow: size
/// overflow`.
fn size_mul_float(a: u64, b: f64) -> VResult<Value> {
    let product = (a as f64) * b;
    let rounded = product.round();
    if rounded.is_nan() {
        return Err(ErrorVal::new("overflow", "size overflow"));
    }
    if rounded < 0.0 {
        return Err(ErrorVal::type_error("size cannot go negative"));
    }
    // `u64::MAX as f64` rounds up to 2^64; reject `>=` so the subsequent cast
    // can never saturate (every value strictly below 2^64 casts exactly).
    if rounded >= u64::MAX as f64 {
        return Err(ErrorVal::new("overflow", "size overflow"));
    }
    Ok(Value::Size(rounded as u64))
}

/// `duration * float` (either operand order). Same saturating-cast hazard as
/// [`size_mul_float`], but durations are signed and negatives are legal, so
/// both over- and under-flow map to `overflow: duration overflow` — exactly
/// what the integer `Duration * int` arm's `checked_mul` produces.
fn duration_mul_float(a: i64, b: f64) -> VResult<Value> {
    let product = (a as f64) * b;
    let rounded = product.round();
    // `i64::MAX as f64` rounds up to 2^63 and `i64::MIN as f64` is exactly
    // -2^63; reject `>= 2^63` and `< -2^63` so the cast can never saturate.
    if rounded.is_nan() || rounded >= i64::MAX as f64 || rounded < i64::MIN as f64 {
        return Err(ErrorVal::new("overflow", "duration overflow"));
    }
    Ok(Value::Duration(rounded as i64))
}

/// Apply an arithmetic/comparison/`in` operator. `&&`/`||`/`??` short-circuit
/// in the evaluator and never reach here.
pub fn binop(op: BinOp, lhs: &Value, rhs: &Value) -> VResult<Value> {
    use BinOp::*;
    use Value::*;

    // Comparing streams is an error (TDD §4.1).
    if matches!(op, Eq | Ne)
        && (matches!(lhs, Stream(_)) || matches!(rhs, Stream(_)))
        && !(matches!(lhs, Stream(_)) && matches!(rhs, Stream(_)))
    {
        return Err(ErrorVal::type_error("cannot compare a stream with a value")
            .with_hint("collect first (`.collect()`)"));
    }

    match op {
        Eq => return Ok(Bool(lhs == rhs)),
        Ne => return Ok(Bool(lhs != rhs)),
        Lt | Le | Gt | Ge => {
            let ord = compare(lhs, rhs)?;
            let b = match op {
                Lt => ord == Ordering::Less,
                Le => ord != Ordering::Greater,
                Gt => ord == Ordering::Greater,
                Ge => ord != Ordering::Less,
                _ => unreachable!(),
            };
            return Ok(Bool(b));
        }
        In => return contains(rhs, lhs).map(Bool),
        _ => {}
    }

    let opname = match op {
        Add => "+",
        Sub => "-",
        Mul => "*",
        Div => "/",
        Rem => "%",
        _ => "?",
    };

    match (lhs, rhs) {
        // --- numeric ---
        (Int(a), Int(b)) => int_op(op, *a, *b),
        (Float(a), Float(b)) => Ok(float_op(op, *a, *b)),
        (Int(a), Float(b)) => Ok(float_op(op, *a as f64, *b)),
        (Float(a), Int(b)) => Ok(float_op(op, *a, *b as f64)),

        // --- str/list concatenation (non-coercive overloads) ---
        (Str(a), Str(b)) if op == Add => Ok(Str(format!("{a}{b}"))),
        (List(a), List(b)) if op == Add => {
            let mut out = a.clone();
            out.extend(b.iter().cloned());
            Ok(List(out))
        }

        // --- size ---
        (Size(a), Size(b)) => match op {
            Add => a
                .checked_add(*b)
                .map(Size)
                .ok_or_else(|| ErrorVal::new("overflow", "size overflow")),
            Sub => a
                .checked_sub(*b)
                .map(Size)
                .ok_or_else(|| ErrorVal::type_error("size cannot go negative")),
            Div => {
                if *b == 0 {
                    Err(ErrorVal::new("div_zero", "division by zero"))
                } else {
                    Ok(Float(*a as f64 / *b as f64))
                }
            }
            _ => Err(type_err(opname, lhs, rhs)),
        },
        (Size(a), Int(b)) => match op {
            Mul => {
                if *b < 0 {
                    Err(ErrorVal::type_error("size cannot go negative"))
                } else {
                    a.checked_mul(*b as u64)
                        .map(Size)
                        .ok_or_else(|| ErrorVal::new("overflow", "size overflow"))
                }
            }
            Div => {
                if *b == 0 {
                    Err(ErrorVal::new("div_zero", "division by zero"))
                } else if *b < 0 {
                    Err(ErrorVal::type_error("size cannot go negative"))
                } else {
                    Ok(Size(a / *b as u64))
                }
            }
            _ => Err(type_err(opname, lhs, rhs)),
        },
        (Int(a), Size(b)) if op == Mul => {
            if *a < 0 {
                Err(ErrorVal::type_error("size cannot go negative"))
            } else {
                (*a as u64)
                    .checked_mul(*b)
                    .map(Size)
                    .ok_or_else(|| ErrorVal::new("overflow", "size overflow"))
            }
        }
        (Size(a), Float(b)) if op == Mul => size_mul_float(*a, *b),
        (Float(a), Size(b)) if op == Mul => size_mul_float(*b, *a),

        // --- duration ---
        // Checked like int/size: unchecked `+`/`*` here PANICKED the whole
        // process (a kernel-hosted eval takes down the daemon) on e.g.
        // `4000000000w + 4000000000w`.
        (Duration(a), Duration(b)) => match op {
            Add => a
                .checked_add(*b)
                .map(Duration)
                .ok_or_else(|| ErrorVal::new("overflow", "duration overflow")),
            Sub => a
                .checked_sub(*b)
                .map(Duration)
                .ok_or_else(|| ErrorVal::new("overflow", "duration overflow")),
            Div => {
                if *b == 0 {
                    Err(ErrorVal::new("div_zero", "division by zero"))
                } else {
                    Ok(Float(*a as f64 / *b as f64))
                }
            }
            _ => Err(type_err(opname, lhs, rhs)),
        },
        (Duration(a), Int(b)) => match op {
            Mul => a
                .checked_mul(*b)
                .map(Duration)
                .ok_or_else(|| ErrorVal::new("overflow", "duration overflow")),
            Div => {
                if *b == 0 {
                    Err(ErrorVal::new("div_zero", "division by zero"))
                } else {
                    Ok(Duration(a / b))
                }
            }
            _ => Err(type_err(opname, lhs, rhs)),
        },
        (Int(a), Duration(b)) if op == Mul => a
            .checked_mul(*b)
            .map(Duration)
            .ok_or_else(|| ErrorVal::new("overflow", "duration overflow")),
        (Duration(a), Float(b)) if op == Mul => duration_mul_float(*a, *b),
        (Float(a), Duration(b)) if op == Mul => duration_mul_float(*b, *a),

        // --- datetime ---
        (DateTime(z), Duration(ns)) => {
            let signed = match op {
                Add => *ns,
                Sub => -*ns,
                _ => return Err(type_err(opname, lhs, rhs)),
            };
            let span = jiff::SignedDuration::from_nanos(signed);
            z.checked_add(span)
                .map(|nz| DateTime(Box::new(nz)))
                .map_err(|e| ErrorVal::new("overflow", format!("datetime out of range: {e}")))
        }
        (Duration(ns), DateTime(z)) if op == Add => {
            let span = jiff::SignedDuration::from_nanos(*ns);
            z.checked_add(span)
                .map(|nz| DateTime(Box::new(nz)))
                .map_err(|e| ErrorVal::new("overflow", format!("datetime out of range: {e}")))
        }
        (DateTime(a), DateTime(b)) if op == Sub => {
            // The i128 ns difference can exceed i64 (~±292 years); a raw
            // `as i64` cast wrapped silently.
            let d = a.timestamp().as_nanosecond() - b.timestamp().as_nanosecond();
            i64::try_from(d)
                .map(Duration)
                .map_err(|_| ErrorVal::new("overflow", "duration overflow"))
        }

        _ => Err(type_err(opname, lhs, rhs)),
    }
}

/// Ordering for `< <= > >=`. Comparable within a kind; int/float mix promotes.
pub fn compare(lhs: &Value, rhs: &Value) -> VResult<Ordering> {
    use Value::*;
    let err = || {
        ErrorVal::type_error(format!(
            "cannot compare {} with {}",
            lhs.type_name(),
            rhs.type_name()
        ))
    };
    match (lhs, rhs) {
        (Int(a), Int(b)) => Ok(a.cmp(b)),
        (Float(a), Float(b)) => a.partial_cmp(b).ok_or_else(err),
        (Int(a), Float(b)) => (*a as f64).partial_cmp(b).ok_or_else(err),
        (Float(a), Int(b)) => a.partial_cmp(&(*b as f64)).ok_or_else(err),
        (Str(a), Str(b)) => Ok(a.cmp(b)),
        (Path(a), Path(b)) => Ok(a.cmp(b)),
        (Size(a), Size(b)) => Ok(a.cmp(b)),
        (Duration(a), Duration(b)) => Ok(a.cmp(b)),
        (DateTime(a), DateTime(b)) => Ok(a.timestamp().cmp(&b.timestamp())),
        (Time(a), Time(b)) => Ok((a.hour, a.min, a.sec).cmp(&(b.hour, b.min, b.sec))),
        (Bool(a), Bool(b)) => Ok(a.cmp(b)),
        _ => Err(err()),
    }
}

/// `needle in haystack` — membership.
fn contains(haystack: &Value, needle: &Value) -> VResult<bool> {
    use Value::*;
    match (haystack, needle) {
        (List(xs), n) => Ok(xs.iter().any(|x| x == n)),
        (Table(rows), Record(r)) => Ok(rows.iter().any(|row| row == r)),
        (Str(h), Str(n)) => Ok(h.contains(n.as_str())),
        (Record(r), Str(k)) => Ok(r.contains_key(k)),
        (Range(r), Int(i)) => Ok(r.contains(*i)),
        _ => Err(ErrorVal::type_error(format!(
            "cannot test {} membership in {}",
            needle.type_name(),
            haystack.type_name()
        ))),
    }
}

/// Unary operators: `!bool`, `-int/float/duration`.
pub fn unop(op: shoal_ast::UnOp, v: &Value) -> VResult<Value> {
    use Value::*;
    match op {
        shoal_ast::UnOp::Not => match v {
            Bool(b) => Ok(Bool(!b)),
            Outcome(o) => Ok(Bool(!o.ok)),
            other => Err(ErrorVal::type_error(format!(
                "cannot negate {}",
                other.type_name()
            ))),
        },
        shoal_ast::UnOp::Neg => match v {
            Int(i) => i
                .checked_neg()
                .map(Int)
                .ok_or_else(|| ErrorVal::new("overflow", "integer overflow")),
            Float(f) => Ok(Float(-f)),
            Duration(d) => Ok(Duration(-d)),
            other => Err(ErrorVal::type_error(format!(
                "cannot apply unary `-` to {}",
                other.type_name()
            ))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shoal_ast::BinOp::*;

    #[test]
    fn matrix() {
        assert_eq!(
            binop(Add, &Value::Int(2), &Value::Int(3)).unwrap(),
            Value::Int(5)
        );
        assert_eq!(
            binop(Add, &Value::Int(1), &Value::Float(0.5)).unwrap(),
            Value::Float(1.5)
        );
        assert_eq!(
            binop(Add, &Value::Size(1000), &Value::Size(500)).unwrap(),
            Value::Size(1500)
        );
        assert_eq!(
            binop(Mul, &Value::Size(1000), &Value::Int(3)).unwrap(),
            Value::Size(3000)
        );
        assert_eq!(
            binop(Div, &Value::Size(1000), &Value::Size(500)).unwrap(),
            Value::Float(2.0)
        );
        assert!(binop(Add, &Value::Size(1), &Value::Int(1)).is_err());
        assert!(binop(Add, &Value::Str("a".into()), &Value::Int(1)).is_err());
        assert_eq!(
            binop(Add, &Value::Str("a".into()), &Value::Str("b".into())).unwrap(),
            Value::Str("ab".into())
        );
        assert_eq!(
            binop(Div, &Value::Int(7), &Value::Int(2)).unwrap(),
            Value::Int(3)
        );
        assert_eq!(
            binop(Div, &Value::Int(1), &Value::Int(0)).unwrap_err().code,
            "div_zero"
        );
        assert_eq!(
            binop(
                Sub,
                &Value::Duration(90_000_000_000),
                &Value::Duration(30_000_000_000)
            )
            .unwrap(),
            Value::Duration(60_000_000_000)
        );
    }

    #[test]
    fn size_duration_times_float_is_checked() {
        // In-range products still round-cast fine.
        assert_eq!(
            binop(Mul, &Value::Size(2000), &Value::Float(1.5)).unwrap(),
            Value::Size(3000)
        );
        assert_eq!(
            binop(Mul, &Value::Float(1.5), &Value::Size(2000)).unwrap(),
            Value::Size(3000)
        );
        // Overflow past u64 -> `overflow: size overflow`, both operand orders.
        let e = binop(
            Mul,
            &Value::Size(9_000_000_000_000_000_000),
            &Value::Float(100.0),
        )
        .unwrap_err();
        assert_eq!(e.code, "overflow");
        assert!(e.msg.contains("size overflow"), "{}", e.msg);
        assert_eq!(
            binop(
                Mul,
                &Value::Float(100.0),
                &Value::Size(9_000_000_000_000_000_000)
            )
            .unwrap_err()
            .code,
            "overflow"
        );
        // Negative product -> `type_error: size cannot go negative`.
        let e = binop(Mul, &Value::Size(1000), &Value::Float(-1.0)).unwrap_err();
        assert_eq!(e.code, "type_error");
        assert!(e.msg.contains("negative"), "{}", e.msg);
        // NaN / infinite products are rejected as overflow, never cast to 0.
        assert_eq!(
            binop(Mul, &Value::Size(1000), &Value::Float(f64::NAN))
                .unwrap_err()
                .code,
            "overflow"
        );
        assert_eq!(
            binop(Mul, &Value::Size(1000), &Value::Float(f64::INFINITY))
                .unwrap_err()
                .code,
            "overflow"
        );
        // Durations are signed: a negative product is legal, not an error.
        assert_eq!(
            binop(Mul, &Value::Duration(5_000_000_000), &Value::Float(-1.0)).unwrap(),
            Value::Duration(-5_000_000_000)
        );
        // Duration overflow past i64 -> `overflow: duration overflow`.
        let e = binop(Mul, &Value::Duration(i64::MAX), &Value::Float(1000.0)).unwrap_err();
        assert_eq!(e.code, "overflow");
        assert!(e.msg.contains("duration overflow"), "{}", e.msg);
        assert_eq!(
            binop(Mul, &Value::Float(1000.0), &Value::Duration(i64::MIN))
                .unwrap_err()
                .code,
            "overflow"
        );
    }

    #[test]
    fn membership() {
        let list = Value::List(vec![Value::Int(1), Value::Int(2)]);
        assert_eq!(binop(In, &Value::Int(2), &list).unwrap(), Value::Bool(true));
        assert_eq!(
            binop(In, &Value::Str("el".into()), &Value::Str("hello".into())).unwrap(),
            Value::Bool(true)
        );
    }

    #[test]
    fn comparisons() {
        assert_eq!(
            binop(Lt, &Value::Int(1), &Value::Float(1.5)).unwrap(),
            Value::Bool(true)
        );
        assert!(binop(Lt, &Value::Int(1), &Value::Str("a".into())).is_err());
    }
}
