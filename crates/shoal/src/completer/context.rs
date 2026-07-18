//! Cursor-context classification using Shoal's modal lexer.

use shoal_syntax::lexer::RESERVED;
use shoal_syntax::{Lexer, Mode, Tok};
use shoal_value::Env;

use super::inference::infer_receiver_type;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Ctx {
    Head {
        start: usize,
        word: String,
    },
    Arg {
        start: usize,
        word: String,
        head: String,
    },
    Expr {
        start: usize,
        word: String,
    },
    Method {
        start: usize,
        word: String,
        recv: Option<String>,
    },
    None,
}

/// Classify the cursor according to Shoal's statement-dispatch approximation.
pub(super) fn classify(env: &Env, line: &str, pos: usize) -> Ctx {
    let pos = pos.min(line.len());
    let stmt_start = statement_start(line, pos);
    let lexer = Lexer::new(line);
    let word0 = lexer.skip_trivia(stmt_start);
    if word0 > pos {
        return Ctx::None;
    }
    if word0 >= pos {
        return Ctx::Head {
            start: pos,
            word: String::new(),
        };
    }
    let Ok((tok0, span0)) = lexer.token(word0, Mode::Expr) else {
        return Ctx::None;
    };
    let (start0, end0) = (span0.start as usize, span0.end as usize);
    match tok0 {
        Tok::Ident(name) => {
            if pos <= end0 {
                return Ctx::Head {
                    start: start0,
                    word: line[start0..pos].to_string(),
                };
            }
            let is_keyword = RESERVED.contains(&name.as_str());
            let is_bound = env.is_bound(&name);
            let is_namespace = shoal_eval::namespace_method_names(&name).next().is_some();
            let is_assign = matches!(
                lexer.token(end0, Mode::Expr),
                Ok((
                    Tok::Eq | Tok::PlusEq | Tok::MinusEq | Tok::StarEq | Tok::SlashEq,
                    _
                ))
            );
            if is_keyword || is_bound || is_namespace || is_assign {
                expr_or_method(env, line, pos)
            } else {
                match cmd_word_at(&lexer, end0, pos, line) {
                    Some((start, word)) => Ctx::Arg {
                        start,
                        word,
                        head: name,
                    },
                    None => Ctx::None,
                }
            }
        }
        _ if pos <= end0 => Ctx::None,
        _ => expr_or_method(env, line, pos),
    }
}

fn expr_or_method(env: &Env, line: &str, pos: usize) -> Ctx {
    let (start, word) = trailing_ident(line, pos);
    if start > 0 && line.as_bytes()[start - 1] == b'.' {
        let recv = infer_receiver_type(env, line, start - 1);
        Ctx::Method { start, word, recv }
    } else {
        Ctx::Expr { start, word }
    }
}

fn trailing_ident(line: &str, pos: usize) -> (usize, String) {
    let mut start = pos;
    for (index, ch) in line[..pos].char_indices().rev() {
        if ch.is_alphanumeric() || ch == '_' {
            start = index;
        } else {
            break;
        }
    }
    (start, line[start..pos].to_string())
}

/// Find the byte offset of the current top-level statement.
pub(super) fn statement_start(line: &str, pos: usize) -> usize {
    let bytes = line.as_bytes();
    let mut depth: i32 = 0;
    let mut quote: Option<u8> = None;
    let mut boundary = 0usize;
    let mut index = 0usize;
    while index < pos {
        let byte = bytes[index];
        if let Some(delimiter) = quote {
            if delimiter == b'"' && byte == b'\\' {
                index += 2;
                continue;
            }
            if byte == delimiter {
                quote = None;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' | b'\'' => quote = Some(byte),
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b';' | b'\n' if depth <= 0 => boundary = index + 1,
            _ => {}
        }
        index += 1;
    }
    boundary.min(pos)
}

fn cmd_word_at(
    lexer: &Lexer,
    mut scan_pos: usize,
    pos: usize,
    line: &str,
) -> Option<(usize, String)> {
    loop {
        let next = lexer.skip_trivia(scan_pos);
        if next >= pos {
            return Some((pos, String::new()));
        }
        let (tok, span) = lexer.token(next, Mode::Cmd).ok()?;
        if matches!(tok, Tok::Eof) {
            return Some((pos, String::new()));
        }
        let (start, end) = (span.start as usize, span.end as usize);
        if end >= pos {
            return Some((start, line[start..pos].to_string()));
        }
        scan_pos = end.max(next + 1);
    }
}
