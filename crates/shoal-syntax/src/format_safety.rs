//! Lossless-format admission while the AST does not retain free trivia.

use crate::{Lexer, Mode, Seg, Tok, format_program};
use shoal_ast::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatRefusal {
    pub span: Span,
    pub message: &'static str,
}

/// Format only when every `#` belongs to a semantic leaf whose source is
/// reconstructed by the formatter. All remaining hashes are comments or a
/// shebang, neither of which the current AST can preserve.
pub fn format_source_preserving_trivia(
    source: &str,
    program: &Program,
) -> Result<String, FormatRefusal> {
    let mut semantic_hash_spans = Vec::new();
    collect_program(program, source, &mut semantic_hash_spans);
    if let Some((offset, _)) = source.match_indices('#').find(|(offset, _)| {
        !semantic_hash_spans
            .iter()
            .any(|span| contains(*span, *offset))
    }) {
        return Err(FormatRefusal {
            span: Span::new(offset, offset + 1),
            message: "formatting skipped because comments and shebangs cannot yet be preserved",
        });
    }
    Ok(format_program(program))
}

fn contains(span: Span, offset: usize) -> bool {
    span.start as usize <= offset && offset < span.end as usize
}

fn add_semantic_span(source: &str, span: Span, spans: &mut Vec<Span>) {
    let start = span.start as usize;
    let end = (span.end as usize).min(source.len());
    if source
        .get(start..end)
        .is_some_and(|text| text.contains('#'))
    {
        spans.push(Span::new(start, end));
    }
}

fn add_exact_text(source: &str, outer: Span, text: &str, spans: &mut Vec<Span>) {
    if !text.contains('#') {
        return;
    }
    let start = outer.start as usize;
    let end = (outer.end as usize).min(source.len());
    if let Some(relative) = source.get(start..end).and_then(|slice| slice.find(text)) {
        spans.push(Span::new(start + relative, start + relative + text.len()));
    }
}

fn collect_program(program: &Program, source: &str, spans: &mut Vec<Span>) {
    for statement in &program.stmts {
        collect_statement(statement, source, spans);
    }
}

fn collect_statement(statement: &Stmt, source: &str, spans: &mut Vec<Span>) {
    match statement {
        Stmt::Let { pattern, init, .. } => {
            collect_pattern(pattern, source, spans);
            collect_expr(init, source, spans);
        }
        Stmt::Fn { decl } => {
            for parameter in &decl.params {
                if let Some(default) = &parameter.default {
                    collect_expr(default, source, spans);
                }
            }
            collect_block(&decl.body, source, spans);
        }
        Stmt::Alias { target, .. } => collect_command(target, source, spans),
        Stmt::Use { path, span } => add_exact_text(source, *span, path, spans),
        Stmt::Assign { target, value, .. } => {
            collect_expr(target, source, spans);
            collect_expr(value, source, spans);
        }
        Stmt::Return { value, .. } => {
            if let Some(value) = value {
                collect_expr(value, source, spans);
            }
        }
        Stmt::For {
            pattern,
            iter,
            body,
            ..
        } => {
            collect_pattern(pattern, source, spans);
            collect_expr(iter, source, spans);
            collect_block(body, source, spans);
        }
        Stmt::While { cond, body, .. } => {
            collect_expr(cond, source, spans);
            collect_block(body, source, spans);
        }
        Stmt::Expr { expr, .. } => collect_expr(expr, source, spans),
        Stmt::Break { .. } | Stmt::Continue { .. } => {}
    }
}

fn collect_block(block: &Block, source: &str, spans: &mut Vec<Span>) {
    for statement in &block.stmts {
        collect_statement(statement, source, spans);
    }
}

fn collect_args(args: &Args, source: &str, spans: &mut Vec<Span>) {
    for value in &args.pos {
        collect_expr(value, source, spans);
    }
    for named in &args.named {
        collect_expr(&named.value, source, spans);
    }
}

fn collect_expr(expression: &Expr, source: &str, spans: &mut Vec<Span>) {
    match expression {
        Expr::Str { span, .. } | Expr::Regex { span, .. } | Expr::DateTime { span, .. } => {
            add_semantic_span(source, *span, spans);
        }
        Expr::StrInterp { parts, span } => {
            collect_interpolated_literal_spans(source, *span, spans);
            for part in parts {
                if let StrPart::Expr { expr } = part {
                    collect_expr(expr, source, spans);
                }
            }
        }
        Expr::Field { recv, .. } => collect_expr(recv, source, spans),
        Expr::Index { recv, index, .. } => {
            collect_expr(recv, source, spans);
            collect_expr(index, source, spans);
        }
        Expr::MethodCall { recv, args, .. } => {
            collect_expr(recv, source, spans);
            collect_args(args, source, spans);
        }
        Expr::FnCall { args, .. } => collect_args(args, source, spans),
        Expr::Cmd { call, .. } => collect_command(call, source, spans),
        Expr::Lambda { params, body, .. } => {
            for parameter in params {
                if let Some(default) = &parameter.default {
                    collect_expr(default, source, spans);
                }
            }
            collect_expr(body, source, spans);
        }
        Expr::List { items, .. } => {
            for item in items {
                collect_expr(item, source, spans);
            }
        }
        Expr::Record { fields, .. } => {
            for field in fields {
                add_exact_text(source, field.span, &field.name, spans);
                collect_expr(&field.value, source, spans);
            }
        }
        Expr::Block { block, .. } => collect_block(block, source, spans),
        Expr::If {
            cond, then, r#else, ..
        } => {
            collect_expr(cond, source, spans);
            collect_block(then, source, spans);
            if let Some(otherwise) = r#else {
                collect_expr(otherwise, source, spans);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            collect_expr(scrutinee, source, spans);
            for arm in arms {
                for pattern in &arm.patterns {
                    collect_pattern(pattern, source, spans);
                }
                if let Some(guard) = &arm.guard {
                    collect_expr(guard, source, spans);
                }
                collect_expr(&arm.body, source, spans);
            }
        }
        Expr::Try {
            body,
            pattern,
            handler,
            ..
        } => {
            collect_block(body, source, spans);
            if let Some(pattern) = pattern {
                collect_pattern(pattern, source, spans);
            }
            collect_block(handler, source, spans);
        }
        Expr::Catch { expr, handler, .. } => {
            collect_expr(expr, source, spans);
            collect_expr(handler, source, spans);
        }
        Expr::With {
            cwd,
            env,
            reef,
            body,
            ..
        } => {
            for value in [cwd, env, reef].into_iter().flatten() {
                collect_expr(value, source, spans);
            }
            collect_block(body, source, spans);
        }
        Expr::Spawn { body, .. } => collect_block(body, source, spans),
        // Interpreter-block payload is semantically opaque to Shoal. Refuse
        // when it contains `#` until the AST carries its exact payload span.
        Expr::LangBlock { .. } => {}
        Expr::Binary { lhs, rhs, .. }
        | Expr::Range {
            start: lhs,
            end: rhs,
            ..
        } => {
            collect_expr(lhs, source, spans);
            collect_expr(rhs, source, spans);
        }
        Expr::Unary { expr, .. } => collect_expr(expr, source, spans),
        Expr::Null { .. }
        | Expr::Bool { .. }
        | Expr::Int { .. }
        | Expr::Float { .. }
        | Expr::Size { .. }
        | Expr::Duration { .. }
        | Expr::Time { .. }
        | Expr::Var { .. } => {}
    }
}

fn collect_interpolated_literal_spans(source: &str, span: Span, spans: &mut Vec<Span>) {
    let start = span.start as usize;
    let Ok((Tok::StrInterp(parts), token_span)) = Lexer::new(source).token(start, Mode::Expr)
    else {
        return;
    };
    let mut cursor = token_span.start as usize;
    for part in parts {
        if let Seg::Expr { start, end } = part {
            add_semantic_span(source, Span::new(cursor, start as usize), spans);
            cursor = end as usize;
        }
    }
    add_semantic_span(source, Span::new(cursor, token_span.end as usize), spans);
}

fn collect_pattern(pattern: &Pattern, source: &str, spans: &mut Vec<Span>) {
    match pattern {
        Pattern::Lit { expr, .. } => collect_expr(expr, source, spans),
        Pattern::Range { start, end, .. } => {
            collect_expr(start, source, spans);
            collect_expr(end, source, spans);
        }
        Pattern::Record { fields, .. } => {
            for field in fields {
                if let Some(pattern) = &field.pattern {
                    collect_pattern(pattern, source, spans);
                }
            }
        }
        Pattern::List { items, .. } => {
            for item in items {
                collect_pattern(item, source, spans);
            }
        }
        Pattern::Wildcard { .. } | Pattern::Bind { .. } | Pattern::Type { .. } => {}
    }
}

fn collect_command(command: &CmdCall, source: &str, spans: &mut Vec<Span>) {
    add_exact_text(source, command.span, &command.head, spans);
    for prefix in &command.env_prefix {
        collect_command_arg(&prefix.value, source, spans);
    }
    for argument in &command.args {
        collect_command_arg(argument, source, spans);
    }
    for redirect in &command.redirects {
        collect_command_arg(&redirect.target, source, spans);
    }
    if let Some(trailing) = &command.trailing {
        collect_block(trailing, source, spans);
    }
}

fn collect_command_arg(argument: &CmdArg, source: &str, spans: &mut Vec<Span>) {
    match argument {
        CmdArg::Word { span, .. } | CmdArg::Path { span, .. } | CmdArg::Glob { span, .. } => {
            add_semantic_span(source, *span, spans);
        }
        CmdArg::Str { expr, .. } | CmdArg::Expr { expr, .. } => {
            collect_expr(expr, source, spans);
        }
        CmdArg::FlagLong { value, .. } => {
            if let Some(value) = value {
                collect_command_arg(value, source, spans);
            }
        }
        CmdArg::FlagShort { .. } | CmdArg::DashDash { .. } | CmdArg::Dash { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;

    #[test]
    fn comments_and_shebangs_refuse_but_semantic_hashes_format() {
        for source in [
            "#!/usr/bin/env shoal\nlet x=1\n",
            "let x=1# keep\n",
            "let x = 1 # keep\n",
            "let x = [1, # keep\n2]\n",
            "let x = \"\"\"{\n1 # keep\n}\n\"\"\"\n",
        ] {
            let program = parse(source).unwrap();
            let refusal = format_source_preserving_trivia(source, &program).unwrap_err();
            assert_eq!(refusal.span.start as usize, source.find('#').unwrap());
        }

        for source in [
            "let x=\"#\"\n",
            "echo ver#2\n",
            "tool#v2 arg\n",
            "use ./module#v2\n",
            "let x={\"#\": 1}\n",
        ] {
            let program = parse(source).unwrap();
            assert!(
                format_source_preserving_trivia(source, &program).is_ok(),
                "{source}"
            );
        }
    }
}
