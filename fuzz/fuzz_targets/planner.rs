#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    let shoal_sh::syntax::ParseStatus::Complete(program) = shoal_sh::syntax::parse_status(source) else {
        return;
    };
    let mut evaluator = shoal_sh::eval::Evaluator::new(std::env::temp_dir());
    let _ = evaluator.plan_program(&program);
});
