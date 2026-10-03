#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    if let shoal_syntax::ParseStatus::Complete(program) = shoal_syntax::parse_status(source) {
        let formatted = shoal_syntax::format_program(&program);
        assert!(matches!(
            shoal_syntax::parse_status(&formatted),
            shoal_syntax::ParseStatus::Complete(_)
        ));
    }
});
