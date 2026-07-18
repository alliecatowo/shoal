//! Receiver-type inference for method completion.

use shoal_syntax::lexer::RESERVED;
use shoal_syntax::{Lexer, Mode, Tok};
use shoal_value::Env;

use super::context::statement_start;

/// Infer a receiver's `Value::type_name` from the expression ending at `.`.
/// Unknown, chained, and computed receivers deliberately return `None`, which
/// keeps completion on the full method union instead of narrowing incorrectly.
pub(super) fn infer_receiver_type(env: &Env, line: &str, dot_pos: usize) -> Option<String> {
    let stmt_start = statement_start(line, dot_pos);
    let toks = expr_tokens(line, stmt_start, dot_pos);
    let (last_tok, _) = toks.last()?;
    match last_tok {
        Tok::Str(_) | Tok::StrInterp(_) => Some("str".into()),
        Tok::Int(_) => Some("int".into()),
        Tok::Float(_) => Some("float".into()),
        Tok::Size(_) => Some("size".into()),
        Tok::Duration(_) => Some("duration".into()),
        Tok::Time { .. } => Some("time".into()),
        Tok::DateTime(_) => Some("datetime".into()),
        Tok::Ident(name) => {
            match name.as_str() {
                "true" | "false" => return Some("bool".into()),
                keyword if RESERVED.contains(&keyword) => return None,
                _ => {}
            }
            if shoal_eval::namespace_method_names(name).next().is_some() {
                return Some(format!("namespace:{name}"));
            }
            // `a.b.` has a computed receiver whose type is not available from
            // lexical context alone.
            if matches!(
                toks.iter().rev().nth(1),
                Some((Tok::Dot | Tok::QuestionDot, _))
            ) {
                return None;
            }
            Some(env.get(name.as_str())?.type_name().to_string())
        }
        Tok::RBracket => bracket_literal_type(&toks, &Tok::LBracket, "list"),
        Tok::RBrace => bracket_literal_type(&toks, &Tok::LBrace, "record"),
        _ => None,
    }
}

/// Lex the receiver in expression mode, excluding tokens crossing the dot.
fn expr_tokens(line: &str, stmt_start: usize, dot_pos: usize) -> Vec<(Tok, usize)> {
    let lexer = Lexer::new(line);
    let mut out = Vec::new();
    let mut scan = stmt_start;
    loop {
        let next = lexer.skip_trivia(scan);
        if next >= dot_pos {
            break;
        }
        let Ok((tok, span)) = lexer.token(next, Mode::Expr) else {
            break;
        };
        let (start, end) = (span.start as usize, span.end as usize);
        if matches!(tok, Tok::Eof) || start >= dot_pos || end > dot_pos {
            break;
        }
        out.push((tok, start));
        scan = end.max(next + 1);
    }
    out
}

/// Distinguish a fresh list/record literal from postfix indexing/application.
fn bracket_literal_type(toks: &[(Tok, usize)], opener: &Tok, ty: &str) -> Option<String> {
    let mut depth = 0i32;
    let mut open_index = None;
    for (index, (tok, _)) in toks.iter().enumerate().rev() {
        match tok {
            Tok::RParen | Tok::RBracket | Tok::RBrace => depth += 1,
            Tok::LParen | Tok::LBracket | Tok::LBrace => {
                depth -= 1;
                if depth == 0 {
                    open_index = Some(index);
                    break;
                }
            }
            _ => {}
        }
    }
    let open_index = open_index?;
    if &toks[open_index].0 != opener {
        return None;
    }
    let previous = open_index.checked_sub(1).map(|index| &toks[index].0);
    if previous.is_some_and(atom_ends) {
        return None;
    }
    Some(ty.to_string())
}

fn atom_ends(tok: &Tok) -> bool {
    matches!(
        tok,
        Tok::Ident(_)
            | Tok::RParen
            | Tok::RBracket
            | Tok::RBrace
            | Tok::Str(_)
            | Tok::StrInterp(_)
            | Tok::Int(_)
            | Tok::Float(_)
            | Tok::Size(_)
            | Tok::Duration(_)
            | Tok::Time { .. }
            | Tok::DateTime(_)
            | Tok::Regex(_)
    )
}
