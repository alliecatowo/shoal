//! JSON conversion (`json_to_value`/`value_to_json`), moved verbatim out of
//! `lib.rs`.

use super::*;

pub fn json_to_value(j: &serde_json::Value) -> Value {
    match j {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
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
