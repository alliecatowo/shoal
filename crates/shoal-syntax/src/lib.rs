//! Modal lexer and parser for shoal.

pub mod lexer;
mod parser;

pub use lexer::{LexError, Lexer, Mode, Seg, Tok};
pub use parser::{ParseError, ParseResult, Parser, parse};
