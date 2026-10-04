#[test]
fn test_caret() {
    println!("{:#?}", shoal_sh::syntax::parse("{ ^echo foo }"));
}
