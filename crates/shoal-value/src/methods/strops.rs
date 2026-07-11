//! String-receiver methods (`.trim`, `.split`, `.matches`, `.str`, …).
//!
//! Named `strops` rather than `str` to avoid shadowing the `str` primitive
//! type in scopes that glob-import this module.

use super::*;

pub(crate) fn string_unary(v: Value, f: impl FnOnce(&str) -> Value) -> VResult<Value> {
    match v {
        Value::Str(s) => Ok(f(&s)),
        v => Err(ErrorVal::type_error(format!(
            "expected str, found {}",
            v.type_name()
        ))),
    }
}
pub(crate) fn string_pred(v: Value, q: &str, f: fn(&str, &str) -> bool) -> VResult<Value> {
    string_unary(v, |s| Value::Bool(f(s, q)))
}
pub(crate) fn matches_method(v: Value, q: &Value) -> VResult<Value> {
    match (v, q) {
        (Value::Str(s), Value::Regex(r)) => Ok(Value::List(
            r.re.find_iter(&s)
                .map(|m| Value::Str(m.as_str().into()))
                .collect(),
        )),
        _ => Err(ErrorVal::type_error(
            "matches expects str receiver and regex",
        )),
    }
}
/// `.replace(pat, rep)` — `pat` is a `str` (literal, all occurrences) or a
/// `regex` (all matches; `$1`/`$name` in `rep` expand capture groups, per the
/// `regex` crate). Mirrors the str/regex duality of `.matches`/`.match`.
pub(crate) fn replace_method(v: Value, pat: &Value, rep: &str) -> VResult<Value> {
    match (v, pat) {
        (Value::Str(s), Value::Str(p)) => Ok(Value::Str(s.replace(p.as_str(), rep))),
        (Value::Str(s), Value::Regex(r)) => {
            Ok(Value::Str(r.re.replace_all(&s, rep).into_owned()))
        }
        (Value::Str(_), other) => Err(ErrorVal::type_error(format!(
            "replace pattern must be str or regex, found {}",
            other.type_name()
        ))),
        (v, _) => Err(ErrorVal::type_error(format!(
            "replace expects a str receiver, found {}",
            v.type_name()
        ))),
    }
}
pub(crate) fn match_method(v: Value, q: &Value) -> VResult<Value> {
    match (v, q) {
        (Value::Str(s), Value::Regex(r)) => Ok(r
            .re
            .find(&s)
            .map(|m| Value::Str(m.as_str().into()))
            .unwrap_or(Value::Null)),
        _ => Err(ErrorVal::type_error("match expects str receiver and regex")),
    }
}
pub(crate) fn string_parse(v: Value, ty: &str) -> VResult<Value> {
    match v {
        Value::Str(s) => match ty {
            "int" => s
                .parse()
                .map(Value::Int)
                .map_err(|_| ErrorVal::arg_error(format!("cannot parse {s:?} as int"))),
            _ => s
                .parse()
                .map(Value::Float)
                .map_err(|_| ErrorVal::arg_error(format!("cannot parse {s:?} as float"))),
        },
        v => Err(ErrorVal::type_error(format!(
            "expected str, found {}",
            v.type_name()
        ))),
    }
}
pub(crate) fn to_str(v: Value, lossy: bool) -> VResult<Value> {
    match v {
        Value::Str(s) => Ok(Value::Str(s)),
        Value::Path(p) => {
            if lossy {
                Ok(Value::Str(p.to_string_lossy().into()))
            } else {
                p.into_os_string()
                    .into_string()
                    .map(Value::Str)
                    .map_err(|_| ErrorVal::new("utf8_error", "path is not valid UTF-8"))
            }
        }
        Value::Bytes(b) => {
            if lossy {
                Ok(Value::Str(String::from_utf8_lossy(&b).into()))
            } else {
                String::from_utf8((*b).clone())
                    .map(Value::Str)
                    .map_err(|_| ErrorVal::new("utf8_error", "bytes are not valid UTF-8"))
            }
        }
        v => Err(ErrorVal::type_error(format!(
            "cannot convert {} to str",
            v.type_name()
        ))),
    }
}
