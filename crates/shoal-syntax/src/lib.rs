//! Modal lexer and parser for shoal.
#![allow(
    clippy::collapsible_if,
    clippy::collapsible_match,
    clippy::if_same_then_else,
    clippy::match_like_matches_macro,
    clippy::possible_missing_else,
    clippy::unnecessary_cast,
    clippy::while_let_loop,
    clippy::approx_constant
)]

mod format;
pub mod lexer;
mod parser;

pub use format::{canonical_equivalent, format_program};
pub use lexer::{LexError, Lexer, Mode, Seg, Tok};
pub use parser::{
    ParseCtx, ParseError, ParseResult, Parser, parse, parse_with_ctx, parse_with_scope,
};

#[derive(Debug)]
pub enum ParseStatus {
    Complete(shoal_ast::Program),
    Incomplete(ParseError),
    Error(ParseError),
}

pub fn parse_status(src: &str) -> ParseStatus {
    match parse(src) {
        Ok(p) => ParseStatus::Complete(p),
        Err(e) if incomplete(src, &e) => ParseStatus::Incomplete(e),
        Err(e) => ParseStatus::Error(e),
    }
}
fn incomplete(src: &str, e: &ParseError) -> bool {
    if e.msg.contains("unterminated")
        || (e.span.end as usize >= src.len() && e.msg.starts_with("expected"))
    {
        return true;
    }
    let (mut stack, mut quote, mut esc) = (Vec::new(), None, false);
    for c in src.chars() {
        if let Some(q) = quote {
            if q == '"' && !esc && c == '\\' {
                esc = true;
                continue;
            }
            if !esc && c == q {
                quote = None
            }
            esc = false;
            continue;
        }
        match c {
            '"' | '\'' => quote = Some(c),
            '(' | '[' | '{' => stack.push(c),
            ')' => {
                if stack.last() == Some(&'(') {
                    stack.pop();
                }
            }
            ']' => {
                if stack.last() == Some(&'[') {
                    stack.pop();
                }
            }
            '}' => {
                if stack.last() == Some(&'{') {
                    stack.pop();
                }
            }
            _ => {}
        }
    }
    quote.is_some()
        || !stack.is_empty()
        || src
            .trim_end()
            .ends_with(['+', '-', '*', '/', '=', ',', '.'])
}

#[cfg(test)]
mod status_tests {
    use super::*;
    #[test]
    fn classifies_incomplete_delimiters_and_strings() {
        for s in ["(", "[1,", "if true {", "\"hello"] {
            assert!(
                matches!(parse_status(s), ParseStatus::Incomplete(_)),
                "{s:?}"
            )
        }
    }
    #[test]
    fn every_prefix_is_safe() {
        for full in [
            "let x = [1, 2, 3]\nx.map(v => v + 1)",
            "fn f(a: int) { if true { a } else { 0 } }",
        ] {
            for i in 0..=full.len() {
                if full.is_char_boundary(i) {
                    let _ = parse_status(&full[..i]);
                }
            }
        }
    }
}
