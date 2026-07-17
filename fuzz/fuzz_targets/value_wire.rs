#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };

    let normalized = shoal_value::value_to_json(&shoal_value::json_to_value(&json));
    let renormalized = shoal_value::value_to_json(&shoal_value::json_to_value(&normalized));
    assert_eq!(renormalized, normalized);
    let _ = serde_json::to_vec(&normalized).unwrap();

    if let Ok(wire) = serde_json::from_value::<shoal_proto::WireValue>(json) {
        let encoded = serde_json::to_value(&wire).unwrap();
        let decoded: shoal_proto::WireValue = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, wire);
    }
});
