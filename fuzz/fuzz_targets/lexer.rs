#![no_main]
use libfuzzer_sys::fuzz_target;
fuzz_target!(|data:&[u8]|{if let Ok(s)=std::str::from_utf8(data){let l=shoal_syntax::Lexer::new(s);let mut p=0;while p<s.len(){match l.token(p,shoal_syntax::Mode::Expr){Ok((_,sp))if sp.end as usize>p=>p=sp.end as usize,_=>break}}}});
