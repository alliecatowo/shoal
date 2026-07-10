#[test]
fn test_caret() {
    println!("{:#?}", shoal_syntax::parse("{ ^echo foo }"));
}
