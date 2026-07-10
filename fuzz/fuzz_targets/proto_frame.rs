#![no_main]
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data:&[u8]|{let mut v=data.to_vec();v.push(b'\n');let _=shoal_proto::read_frame(&mut std::io::Cursor::new(v));});
