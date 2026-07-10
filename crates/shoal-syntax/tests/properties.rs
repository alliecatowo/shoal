use proptest::prelude::*;
use shoal_syntax::{canonical_equivalent, format_program, parse, parse_status};
proptest! {
 #![proptest_config(ProptestConfig::with_cases(128))]
 #[test] fn parser_never_panics(src in any::<String>()){let _=parse(&src);let _=parse_status(&src);}
 #[test] fn every_utf8_prefix_safe(src in any::<String>()){for i in 0..=src.len(){if src.is_char_boundary(i){let _=parse_status(&src[..i]);}}}
 #[test] fn generated_ast_format_idempotent(values in prop::collection::vec(-10_000i64..10_000,1..12),ops in prop::collection::vec(prop_oneof![Just("+"),Just("-"),Just("*")],0..11)){let mut source=values[0].to_string();for(op,value)in ops.iter().zip(values.iter().skip(1)){source=format!("({source} {op} {value})")}let ast=parse(&source).unwrap();let once=format_program(&ast);let reparsed=parse(&once).unwrap();prop_assert!(canonical_equivalent(&ast,&reparsed));prop_assert_eq!(format_program(&reparsed),once);}
}
