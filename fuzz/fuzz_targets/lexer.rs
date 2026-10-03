#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(source) = std::str::from_utf8(data) else {
        return;
    };
    let lexer = shoal_syntax::Lexer::new(source);
    for mode in [shoal_syntax::Mode::Expr, shoal_syntax::Mode::Cmd] {
        let mut offset = 0;
        while offset < source.len() {
            match lexer.token(offset, mode) {
                Ok((_, span)) if span.end as usize > offset => offset = span.end as usize,
                _ => break,
            }
        }
    }
});
