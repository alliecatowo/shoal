//! JSON conversion (`json_to_value`/`value_to_json`), moved verbatim out of
//! `lib.rs`.

use super::*;

pub fn json_to_value(j: &serde_json::Value) -> Value {
    match j {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            // KNOWN LIMITATION (design decision needed, see FIX 5): `Value::Int`
            // is `i64` and shoal has no bignum (TDD §193). A JSON integer in
            // (i64::MAX, u64::MAX] — e.g. a 64-bit unsigned id like
            // 18446744073709551615 — does not fit i64, so it falls through to
            // `Value::Float`, which loses integer precision above 2^53. There
            // is no lossless representation for it within `Value` today: the
            // only honest alternatives are this lossy float or an out-of-range
            // parse error, and neither is a clear win (erroring would reject an
            // otherwise-valid JSON document). `as_i64()` already accepts every
            // value that genuinely fits i64, so this is not a missed-case bug —
            // it is a real type-system limitation left for a deliberate design
            // call (add a u64/bigint Value variant, or define an explicit
            // policy) rather than papered over with a hack here.
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else {
                Value::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        serde_json::Value::String(s) => Value::Str(s.clone()),
        serde_json::Value::Array(xs) => {
            let vals: Vec<Value> = xs.iter().map(json_to_value).collect();
            // A uniform non-empty array of objects is a table.
            if !vals.is_empty() && vals.iter().all(|v| matches!(v, Value::Record(_))) {
                Value::Table(
                    vals.into_iter()
                        .map(|v| match v {
                            Value::Record(r) => r,
                            _ => unreachable!(),
                        })
                        .collect(),
                )
            } else {
                Value::List(vals)
            }
        }
        serde_json::Value::Object(m) => Value::Record(
            m.iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        ),
    }
}

pub fn value_to_json(v: &Value) -> serde_json::Value {
    use serde_json::json;
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => json!(b),
        Value::Int(i) => json!(i),
        Value::Float(f) => json!(f),
        Value::Str(s) => json!(s),
        Value::Path(p) => json!(p.to_string_lossy()),
        Value::Glob(g) => json!(g.pattern),
        Value::Regex(r) => json!(r.src),
        Value::Size(n) => json!(n),
        Value::Duration(ns) => json!(ns),
        Value::DateTime(z) => json!(z.to_string()),
        Value::Time(t) => json!(render::render_time(t)),
        Value::Bytes(b) => json!(String::from_utf8_lossy(b)),
        // Load the full CAS-backed content (falling back to the bounded preview
        // if the store is unreachable) so JSON serialization is faithful.
        Value::CasBytes(c) => json!(String::from_utf8_lossy(
            &c.resolve().unwrap_or_else(|_| c.preview.as_ref().clone())
        )),
        Value::List(xs) => serde_json::Value::Array(xs.iter().map(value_to_json).collect()),
        Value::Record(r) => serde_json::Value::Object(
            r.iter()
                .map(|(k, v)| (k.clone(), value_to_json(v)))
                .collect(),
        ),
        Value::Table(rows) => serde_json::Value::Array(
            rows.iter()
                .map(|r| {
                    serde_json::Value::Object(
                        r.iter()
                            .map(|(k, v)| (k.clone(), value_to_json(v)))
                            .collect(),
                    )
                })
                .collect(),
        ),
        Value::Range(r) => serde_json::Value::Array(r.iter().map(|i| json!(i)).collect()),
        Value::Outcome(o) => json!({
            "status": o.status, "ok": o.ok,
            "out": String::from_utf8_lossy(&o.stdout),
            "err": String::from_utf8_lossy(&o.stderr),
        }),
        Value::Error(e) => json!({"code": e.code, "msg": e.msg}),
        Value::Secret(s) => json!(format!("secret({})", s.name)),
        other => json!(render::render_inline(other)),
    }
}
