use proptest::prelude::*;
use shoal_proto::{JSONRPC, Request, WirePath, WireValue};
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
        any::<String>().prop_map(|v| WireValue::Str { v })
    ]
}
fn wire_value() -> impl Strategy<Value = WireValue> {
    leaf().prop_recursive(3, 64, 8, |inner| {
        prop::collection::vec(inner, 0..8).prop_map(|v| WireValue::List { v })
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
