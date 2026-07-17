use proptest::prelude::*;
use shoal_proto::{JSONRPC, Request, WirePath, WireSpan, WireValue};
use std::collections::BTreeMap;
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
proptest! {
 #![proptest_config(ProptestConfig::with_cases(128))]
 #[test]fn tagged_value_roundtrip(value in wire_value()){let bytes=serde_json::to_vec(&value).unwrap();let back:WireValue=serde_json::from_slice(&bytes).unwrap();prop_assert_eq!(back,value);}
 #[test]fn frame_decode_never_panics(bytes in prop::collection::vec(any::<u8>(),0..4096)){let mut framed=bytes;framed.push(b'\n');let _=shoal_proto::read_frame(&mut std::io::Cursor::new(framed));}
 #[cfg(unix)]#[test]fn unix_path_bytes_roundtrip(bytes in prop::collection::vec(any::<u8>(),0..512)){let path=OsString::from_vec(bytes);prop_assert_eq!(WirePath::encode(&path).decode().unwrap(),path);}
}
fn leaf() -> impl Strategy<Value = WireValue> {
    prop_oneof![
        Just(WireValue::Null),
        any::<bool>().prop_map(|v| WireValue::Bool { v }),
        any::<i64>().prop_map(|v| WireValue::Int { v }),
        any::<u64>().prop_map(|v| WireValue::Size { v }),
        any::<String>().prop_map(|v| WireValue::Str { v }),
        any::<String>().prop_map(|v| WireValue::DateTime { v }),
        any::<String>().prop_map(|v| WireValue::Time { v }),
        any::<String>().prop_map(|pattern| WireValue::Glob { pattern }),
        any::<String>().prop_map(|src| WireValue::Regex { src }),
        any::<String>().prop_map(|repr| WireValue::Closure { repr }),
        any::<String>().prop_map(|label| WireValue::Stream {
            label,
            cursor: None
        }),
        any::<String>().prop_map(|name| WireValue::Secret { name }),
        any::<String>().prop_map(|repr| WireValue::Cmd { repr }),
        (any::<i64>(), any::<i64>(), any::<bool>()).prop_map(|(start, end, inclusive)| {
            WireValue::Range {
                start,
                end,
                inclusive,
            }
        }),
        (any::<u64>(), any::<bool>()).prop_map(|(id, done)| WireValue::Task { id, done }),
        error_strategy(),
    ]
}
fn error_strategy() -> impl Strategy<Value = WireValue> {
    (
        any::<String>(),
        any::<String>(),
        proptest::option::of((any::<u32>(), any::<u32>())),
        proptest::option::of(any::<String>()),
        proptest::option::of(any::<String>()),
    )
        .prop_map(|(code, msg, span, hint, stderr)| WireValue::Error {
            code,
            msg,
            span: span.map(|(start, end)| WireSpan { start, end }),
            hint,
            stderr,
        })
}
fn wire_value() -> impl Strategy<Value = WireValue> {
    leaf().prop_recursive(3, 64, 8, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..8).prop_map(|v| WireValue::List { v }),
            prop::collection::btree_map(
                prop::string::string_regex("[a-z]{1,6}").unwrap(),
                inner.clone(),
                0..4
            )
            .prop_map(|v| WireValue::Record { v }),
            (
                inner.clone(),
                any::<Option<i32>>(),
                any::<bool>(),
                any::<Option<String>>(),
                any::<bool>(),
                any::<String>(),
                any::<i64>(),
                any::<u32>(),
                any::<String>(),
            )
                .prop_map(
                    |(out, status, ok, signal, streamed, err, dur_ns, pid, cmd)| {
                        WireValue::Outcome {
                            status,
                            ok,
                            signal,
                            streamed,
                            out: Box::new(out),
                            err,
                            dur_ns,
                            pid,
                            cmd,
                            span: None,
                        }
                    }
                ),
            prop::collection::vec(
                (
                    prop::string::string_regex("[a-z]{1,6}").unwrap(),
                    prop::collection::vec(inner.clone(), 0..3)
                ),
                0..3
            )
            .prop_map(|cols| {
                let n = cols.first().map(|(_, v)| v.len()).unwrap_or(0);
                WireValue::Table {
                    cols: cols.into_iter().collect::<BTreeMap<_, _>>(),
                    n,
                }
            }),
            (
                any::<String>(),
                any::<String>(),
                any::<usize>(),
                proptest::option::of(prop::collection::btree_map(
                    prop::string::string_regex("[a-z]{1,6}").unwrap(),
                    any::<String>(),
                    0..4,
                )),
                inner.clone(),
                any::<String>(),
            )
                .prop_map(|(uri, of, n, cols, preview, render_head)| WireValue::Ref {
                    uri,
                    of,
                    n,
                    cols,
                    preview: Box::new(preview),
                    render_head,
                }),
        ]
    })
}
#[test]
fn request_smoke() {
    let r = Request {
        jsonrpc: JSONRPC.into(),
        id: 1.into(),
        method: "parse".into(),
        params: serde_json::json!({"src":"1"}),
    };
    let mut out = vec![];
    shoal_proto::write_frame(&mut out, &r).unwrap();
    assert!(
        shoal_proto::read_frame(&mut std::io::Cursor::new(out))
            .unwrap()
            .is_some()
    )
}
