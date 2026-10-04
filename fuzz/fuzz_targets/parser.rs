#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    if let shoal_sh::syntax::ParseStatus::Complete(program) = shoal_sh::syntax::parse_status(source) {
        let formatted = shoal_sh::syntax::format_program(&program);
        assert!(matches!(
            shoal_sh::syntax::parse_status(&formatted),
            shoal_sh::syntax::ParseStatus::Complete(_)
        ));
    }
});
