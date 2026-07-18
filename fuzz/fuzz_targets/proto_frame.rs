#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut cursor = std::io::Cursor::new(data);
    for _ in 0..64 {
        let before = cursor.position();
        match shoal_proto::read_frame(&mut cursor) {
            Ok(Some(request)) => {
                assert!(cursor.position() > before);
                let mut encoded = Vec::new();
                shoal_proto::write_frame(&mut encoded, &request).unwrap();
                let decoded = shoal_proto::read_frame(&mut std::io::Cursor::new(encoded))
                    .unwrap()
                    .unwrap();
                assert_eq!(decoded, request);
            }
            Ok(None) | Err(_) => break,
        }
    }
});
