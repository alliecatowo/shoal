//! Fallible JSON conversion plus exact source-token numeric admission.

use super::*;

mod number_tokens;
pub use number_tokens::preflight_json_numbers;

pub fn json_to_value(j: &serde_json::Value) -> VResult<Value> {
    Ok(match j {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else if n.as_u64().is_some() {
                return Err(json_number_range(n));
            } else if let Some(value) = n.as_f64().filter(|value| value.is_finite()) {
                Value::Float(value)
            } else {
                return Err(json_number_range(n));
            }
        }
        serde_json::Value::String(s) => Value::Str(s.clone()),
        serde_json::Value::Array(xs) => {
            let vals = xs.iter().map(json_to_value).collect::<VResult<Vec<_>>>()?;
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
                .map(|(key, value)| Ok((key.clone(), json_to_value(value)?)))
                .collect::<VResult<_>>()?,
        ),
    })
}

fn json_number_range(number: &serde_json::Number) -> ErrorVal {
    ErrorVal::new(
        "number_range",
        format!(
            "JSON number `{number}` is outside Shoal's signed 64-bit integer / finite-float range"
        ),
    )
    .with_hint("encode integer identifiers outside the signed 64-bit range as JSON strings")
}

pub fn value_to_json(v: &Value) -> VResult<serde_json::Value> {
    use serde_json::json;
    Ok(match v {
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
        // A CAS-backed bytes value reached here may be bare or nested inside a
        // record/table/list field. This is the single, deliberate, bounded
        // answer for direct encoders (see
        // `CasBytesVal::json_preview`'s doc comment): metadata + the resident
        // preview, never a CAS load. A bare `.json()` METHOD call resolves
        // through method dispatch only when the declared blob length fits its
        // eager wall; larger blobs fail before opening.
        Value::CasBytes(c) => c.json_preview(),
        Value::List(xs) => {
            serde_json::Value::Array(xs.iter().map(value_to_json).collect::<VResult<Vec<_>>>()?)
        }
        Value::Record(r) => serde_json::Value::Object(
            r.iter()
                .map(|(k, v)| Ok((k.clone(), value_to_json(v)?)))
                .collect::<VResult<_>>()?,
        ),
        Value::Table(rows) => serde_json::Value::Array(
            rows.iter()
                .map(|r| {
                    Ok(serde_json::Value::Object(
                        r.iter()
                            .map(|(k, v)| Ok((k.clone(), value_to_json(v)?)))
                            .collect::<VResult<_>>()?,
                    ))
                })
                .collect::<VResult<Vec<_>>>()?,
        ),
        Value::Range(r) => {
            let len = r.materialization_len()?;
            let mut values = Vec::with_capacity(len);
            values.extend(r.iter().map(|value| json!(value)));
            serde_json::Value::Array(values)
        }
        Value::Outcome(o) => json!({
            "status": o.status, "ok": o.ok,
            "out": String::from_utf8_lossy(&o.stdout),
            "err": String::from_utf8_lossy(&o.stderr),
        }),
        Value::Error(e) => json!({"code": e.code, "msg": e.msg}),
        Value::Secret(s) => json!(format!("secret({})", s.name)),
        other => json!(render::render_inline(other)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value_types::test_support;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A CasBytes value NESTED inside a record
    /// (the shape a real spilled `.stdout` takes once captured into a field)
    /// no longer silently pulls the full content through the CAS just because
    /// the record got `value_to_json`'d — it gets the same bounded
    /// ref+preview `json_preview` shape a top-level render would show.
    #[test]
    fn nested_cas_bytes_does_not_load_when_json_encoding_a_record() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = test_support::cas_bytes(b"hel", b"hello world", calls.clone());
        let mut r = Record::new();
        r.insert("out".into(), Value::CasBytes(Arc::new(c)));
        r.insert("status".into(), Value::Int(0));

        let j = value_to_json(&Value::Record(r)).unwrap();
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "nested encode must not load"
        );
        assert_eq!(j["out"]["$"], "bytes_ref");
        assert_eq!(j["out"]["ref"], "val:blake3:deadbeefcafef00d");
        assert_eq!(j["out"]["len"], 11);
        assert_eq!(j["out"]["preview"], "hel");
        assert_eq!(j["status"], 0);
    }

    /// Same fix applies to a table row and a plain list element — any
    /// container, not just a record field.
    #[test]
    fn nested_cas_bytes_does_not_load_inside_list_or_table() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = test_support::cas_bytes(b"a", b"abcdef", calls.clone());
        let list = Value::List(vec![Value::Int(1), Value::CasBytes(Arc::new(c))]);
        let j = value_to_json(&list).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(j[1]["$"], "bytes_ref");
        assert_eq!(j[1]["len"], 6);
    }

    /// A bare CasBytes value handed directly to `value_to_json` (the path
    /// `json.stringify`/`yaml.stringify`/`toml.stringify` take when called
    /// with the value itself, as opposed to the `.json()` VALUE METHOD)
    /// is the same bounded answer — there is exactly one `value_to_json`
    /// behavior for this type, not a top-level/nested split.
    #[test]
    fn bare_value_to_json_on_cas_bytes_is_also_bounded() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = test_support::cas_bytes(b"hel", b"hello world", calls.clone());
        let j = value_to_json(&Value::CasBytes(Arc::new(c))).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(j["$"], "bytes_ref");
        assert_eq!(j["len"], 11);
    }

    #[test]
    fn json_to_value_rejects_unsigned_integers_instead_of_rounding() {
        let number = serde_json::Value::Number(serde_json::Number::from(u64::MAX));
        let error = json_to_value(&number).unwrap_err();
        assert_eq!(error.code, "number_range");
        assert!(error.msg.contains("18446744073709551615"));
    }
}
