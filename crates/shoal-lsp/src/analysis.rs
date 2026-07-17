use shoal_ast::{Block, Expr, Pattern, Program, Span, Stmt, StrPart};
use tower_lsp::lsp_types::{DocumentSymbol, SymbolKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SymbolFlavor {
    Binding,
    MutableBinding,
    Function,
    Parameter,
    Alias,
}

#[derive(Debug, Clone)]
pub(crate) struct Symbol {
    pub name: String,
    pub span: Span,
    pub flavor: SymbolFlavor,
    pub detail: String,
    pub doc: Option<String>,
    pub scope: Span,
}

impl Symbol {
    pub fn completion_kind(&self) -> tower_lsp::lsp_types::CompletionItemKind {
        use tower_lsp::lsp_types::CompletionItemKind;
        match self.flavor {
            SymbolFlavor::Function => CompletionItemKind::FUNCTION,
            SymbolFlavor::Alias => CompletionItemKind::REFERENCE,
            SymbolFlavor::Parameter => CompletionItemKind::VARIABLE,
            SymbolFlavor::Binding | SymbolFlavor::MutableBinding => CompletionItemKind::VARIABLE,
        }
    }

    pub fn symbol_kind(&self) -> SymbolKind {
        match self.flavor {
            SymbolFlavor::Function => SymbolKind::FUNCTION,
            SymbolFlavor::Alias => SymbolKind::KEY,
            SymbolFlavor::Parameter => SymbolKind::VARIABLE,
            SymbolFlavor::Binding | SymbolFlavor::MutableBinding => SymbolKind::VARIABLE,
        }
    }
}

pub(crate) fn collect_symbols(program: &Program, text: &str) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    walk_block_like(&program.stmts, text, Span::new(0, text.len()), &mut symbols);
    symbols.sort_by_key(|symbol| symbol.span.start);
    symbols
}

fn walk_block_like(stmts: &[Stmt], text: &str, scope: Span, symbols: &mut Vec<Symbol>) {
    for stmt in stmts {
        match stmt {
            Stmt::Let {
                pattern,
                mutable,
                init,
                ..
            } => {
                pattern_symbols(
                    pattern,
                    if *mutable {
                        SymbolFlavor::MutableBinding
                    } else {
                        SymbolFlavor::Binding
                    },
                    scope,
                    symbols,
                );
                walk_expr(init, text, symbols);
            }
            Stmt::Fn { decl } => {
                symbols.push(Symbol {
                    name: decl.name.clone(),
                    span: identifier_span(text, decl.span, &decl.name),
                    flavor: SymbolFlavor::Function,
                    detail: format!("fn {}({} params)", decl.name, decl.params.len()),
                    doc: decl.doc.clone(),
                    scope,
                });
                for param in &decl.params {
                    symbols.push(Symbol {
                        name: param.name.clone(),
                        span: identifier_span(text, param.span, &param.name),
                        flavor: SymbolFlavor::Parameter,
                        detail: "function parameter".into(),
                        doc: None,
                        scope: decl.body.span,
                    });
                    if let Some(default) = &param.default {
                        walk_expr(default, text, symbols);
                    }
                }
                walk_block_like(&decl.body.stmts, text, decl.body.span, symbols);
            }
            Stmt::Alias { name, span, .. } => symbols.push(Symbol {
                name: name.clone(),
                span: identifier_span(text, *span, name),
                flavor: SymbolFlavor::Alias,
                detail: "command alias".into(),
                doc: None,
                scope,
            }),
            Stmt::Assign { target, value, .. } => {
                walk_expr(target, text, symbols);
                walk_expr(value, text, symbols);
            }
            Stmt::Return { value, .. } => {
                if let Some(value) = value {
                    walk_expr(value, text, symbols);
                }
            }
            Stmt::For {
                pattern,
                iter,
                body,
                ..
            } => {
                pattern_symbols(pattern, SymbolFlavor::Binding, body.span, symbols);
                walk_expr(iter, text, symbols);
                walk_block_like(&body.stmts, text, body.span, symbols);
            }
            Stmt::While { cond, body, .. } => {
                walk_expr(cond, text, symbols);
                walk_block_like(&body.stmts, text, body.span, symbols);
            }
            Stmt::Expr { expr, .. } => walk_expr(expr, text, symbols),
            Stmt::Use { .. } | Stmt::Break { .. } | Stmt::Continue { .. } => {}
        }
    }
}

fn pattern_symbols(pattern: &Pattern, flavor: SymbolFlavor, scope: Span, out: &mut Vec<Symbol>) {
    match pattern {
        Pattern::Bind { name, span } => out.push(Symbol {
            name: name.clone(),
            span: *span,
            flavor,
            detail: match flavor {
                SymbolFlavor::MutableBinding => "mutable binding",
                _ => "immutable binding",
            }
            .into(),
            doc: None,
            scope,
        }),
        Pattern::Type {
            name: Some(name),
            span,
            ..
        } => out.push(Symbol {
            name: name.clone(),
            span: *span,
            flavor,
            detail: "typed binding".into(),
            doc: None,
            scope,
        }),
        Pattern::Record { fields, .. } => {
            for field in fields {
                if let Some(pattern) = &field.pattern {
                    pattern_symbols(pattern, flavor, scope, out);
                }
            }
        }
        Pattern::List { items, .. } => {
            for pattern in items {
                pattern_symbols(pattern, flavor, scope, out);
            }
        }
        _ => {}
    }
}

fn walk_block(block: &Block, text: &str, out: &mut Vec<Symbol>) {
    walk_block_like(&block.stmts, text, block.span, out);
}

fn walk_expr(expr: &Expr, text: &str, out: &mut Vec<Symbol>) {
    match expr {
        Expr::StrInterp { parts, .. } => {
            for part in parts {
                if let StrPart::Expr { expr } = part {
                    walk_expr(expr, text, out);
                }
            }
        }
        Expr::Field { recv, .. } | Expr::Unary { expr: recv, .. } => walk_expr(recv, text, out),
        Expr::Index { recv, index, .. } => {
            walk_expr(recv, text, out);
            walk_expr(index, text, out);
        }
        Expr::MethodCall { recv, args, .. } => {
            walk_expr(recv, text, out);
            for arg in args
                .pos
                .iter()
                .chain(args.named.iter().map(|arg| &arg.value))
            {
                walk_expr(arg, text, out);
            }
        }
        Expr::FnCall { args, .. } => {
            for arg in args
                .pos
                .iter()
                .chain(args.named.iter().map(|arg| &arg.value))
            {
                walk_expr(arg, text, out);
            }
        }
        Expr::Lambda { params, body, .. } => {
            for param in params {
                out.push(Symbol {
                    name: param.name.clone(),
                    span: identifier_span(text, param.span, &param.name),
                    flavor: SymbolFlavor::Parameter,
                    detail: "lambda parameter".into(),
                    doc: None,
                    scope: expr.span(),
                });
            }
            walk_expr(body, text, out);
        }
        Expr::List { items, .. } => {
            for item in items {
                walk_expr(item, text, out);
            }
        }
        Expr::Record { fields, .. } => {
            for field in fields {
                walk_expr(&field.value, text, out);
            }
        }
        Expr::Block { block, .. } | Expr::Spawn { body: block, .. } => walk_block(block, text, out),
        Expr::If {
            cond, then, r#else, ..
        } => {
            walk_expr(cond, text, out);
            walk_block(then, text, out);
            if let Some(other) = r#else {
                walk_expr(other, text, out);
            }
        }
        Expr::Match {
            scrutinee, arms, ..
        } => {
            walk_expr(scrutinee, text, out);
            for arm in arms {
                for pattern in &arm.patterns {
                    pattern_symbols(pattern, SymbolFlavor::Binding, arm.span, out);
                }
                if let Some(guard) = &arm.guard {
                    walk_expr(guard, text, out);
                }
                walk_expr(&arm.body, text, out);
            }
        }
        Expr::Try {
            body,
            pattern,
            handler,
            ..
        } => {
            walk_block(body, text, out);
            if let Some(pattern) = pattern {
                pattern_symbols(pattern, SymbolFlavor::Binding, handler.span, out);
            }
            walk_block(handler, text, out);
        }
        Expr::Catch {
            expr,
            binder,
            handler,
            span,
        } => {
            walk_expr(expr, text, out);
            if let Some(name) = binder {
                out.push(Symbol {
                    name: name.clone(),
                    span: identifier_span(text, *span, name),
                    flavor: SymbolFlavor::Binding,
                    detail: "error binding".into(),
                    doc: None,
                    scope: handler.span(),
                });
            }
            walk_expr(handler, text, out);
        }
        Expr::With {
            cwd,
            env,
            reef,
            body,
            ..
        } => {
            for value in [cwd, env, reef].into_iter().flatten() {
                walk_expr(value, text, out);
            }
            walk_block(body, text, out);
        }
        Expr::Binary { lhs, rhs, .. }
        | Expr::Range {
            start: lhs,
            end: rhs,
            ..
        } => {
            walk_expr(lhs, text, out);
            walk_expr(rhs, text, out);
        }
        Expr::Null { .. }
        | Expr::Bool { .. }
        | Expr::Int { .. }
        | Expr::Float { .. }
        | Expr::Str { .. }
        | Expr::Size { .. }
        | Expr::Duration { .. }
        | Expr::Time { .. }
        | Expr::DateTime { .. }
        | Expr::Regex { .. }
        | Expr::Var { .. }
        | Expr::Cmd { .. }
        | Expr::LangBlock { .. } => {}
    }
}

fn identifier_span(text: &str, within: Span, name: &str) -> Span {
    let start = within.start as usize;
    let end = (within.end as usize).min(text.len());
    text.get(start..end)
        .and_then(|slice| slice.find(name).map(|offset| start + offset))
        .map_or(within, |start| Span::new(start, start + name.len()))
}

#[allow(deprecated)]
pub(crate) fn document_symbols(
    text: &str,
    symbols: &[Symbol],
    range: impl Fn(&str, Span) -> tower_lsp::lsp_types::Range,
) -> Vec<DocumentSymbol> {
    symbols
        .iter()
        .filter(|symbol| symbol.flavor != SymbolFlavor::Parameter)
        .map(|symbol| DocumentSymbol {
            name: symbol.name.clone(),
            detail: Some(symbol.detail.clone()),
            kind: symbol.symbol_kind(),
            tags: None,
            deprecated: None,
            range: range(text, symbol.span),
            selection_range: range(text, symbol.span),
            children: None,
        })
        .collect()
}
