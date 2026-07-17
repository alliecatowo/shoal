//! JSON conversion (`json_to_value`/`value_to_json`), moved verbatim out of
//! `lib.rs`.

use super::*;

pub fn json_to_value(j: &serde_json::Value) -> Value {
    match j {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            // KNOWN LIMITATION (a deliberate type-system decision remains): `Value::Int`
            // is `i64` and shoal has no bignum (site/content/internals/language-conformance-contract.md). A JSON integer in
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
        // A CAS-backed bytes value reached here is, by construction, either a
        // bare top-level `resolve()`-first call (`json.stringify`/
        // `yaml.stringify`/`toml.stringify` handed the value directly) or
        // NESTED inside a record/table/list field. Either way this is the
        // single, deliberate, bounded answer (see
        // `CasBytesVal::json_preview`'s doc comment): metadata + the resident
        // preview, never a CAS load. A bare `.json()` METHOD call never
        // reaches this arm at all — `methods::dispatch`'s CasBytes fallback
        // fully materializes and converts to `Value::Bytes` first, so that
        // call site's full-fidelity behavior is unchanged.
        Value::CasBytes(c) => c.json_preview(),
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

        let j = value_to_json(&Value::Record(r));
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
        let j = value_to_json(&list);
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
        let j = value_to_json(&Value::CasBytes(Arc::new(c)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(j["$"], "bytes_ref");
        assert_eq!(j["len"], 11);
    }
}
