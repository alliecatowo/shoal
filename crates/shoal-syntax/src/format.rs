use shoal_ast::*;

pub fn format_program(p: &Program) -> String {
    p.stmts.iter().map(stmt).collect::<Vec<_>>().join("\n") + "\n"
}
pub fn canonical_equivalent(a: &Program, b: &Program) -> bool {
    fn strip(v: &mut serde_json::Value) {
        match v {
            serde_json::Value::Object(m) => {
                m.remove("span");
                for v in m.values_mut() {
                    strip(v)
                }
            }
            serde_json::Value::Array(a) => {
                for v in a {
                    strip(v)
                }
            }
            _ => {}
        }
    }
    let (mut a, mut b) = (
        serde_json::to_value(a).unwrap(),
        serde_json::to_value(b).unwrap(),
    );
    strip(&mut a);
    strip(&mut b);
    a == b
}
fn stmt(s: &Stmt) -> String {
    match s {
        Stmt::Let {
            pattern,
            ty,
            init,
            mutable,
            exported,
            ..
        } => format!(
            "{}{} {}{} = {}",
            if *exported { "export " } else { "" },
            if *mutable { "var" } else { "let" },
            pat(pattern),
            ty.as_ref()
                .map(|t| format!(": {}", typ(t)))
                .unwrap_or_default(),
            expr(init)
        ),
        Stmt::Fn { decl: d } => format!(
            "{}fn {}({}){} {}",
            if d.exported { "export " } else { "" },
            d.name,
            params(&d.params, d.rest.as_ref()),
            d.ret
                .as_ref()
                .map(|t| format!(" -> {}", typ(t)))
                .unwrap_or_default(),
            block(&d.body)
        ),
        Stmt::Alias { name, target, .. } => format!("alias {name} = {}", cmd(target)),
        Stmt::Use { path, .. } => format!("use {path}"),
        Stmt::Assign {
            target, op, value, ..
        } => format!(
            "{} {} {}",
            expr(target),
            match op {
                AssignOp::Set => "=",
                AssignOp::Add => "+=",
                AssignOp::Sub => "-=",
                AssignOp::Mul => "*=",
                AssignOp::Div => "/=",
            },
            expr(value)
        ),
        Stmt::Return { value, .. } => format!(
            "return{}",
            value
                .as_ref()
                .map(|v| format!(" {}", expr(v)))
                .unwrap_or_default()
        ),
        Stmt::Break { .. } => "break".into(),
        Stmt::Continue { .. } => "continue".into(),
        Stmt::For {
            pattern,
            iter,
            body,
            ..
        } => format!("for {} in {} {}", pat(pattern), expr(iter), block(body)),
        Stmt::While { cond, body, .. } => format!("while {} {}", expr(cond), block(body)),
        Stmt::Expr { expr: e, .. } => expr(e),
    }
}
fn block(b: &Block) -> String {
    if b.stmts.is_empty() {
        "{}".into()
    } else {
        format!(
            "{{\n{}\n}}",
            b.stmts
                .iter()
                .map(|s| format!("  {}", stmt(s).replace('\n', "\n  ")))
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
}
fn typ(t: &Type) -> String {
    format!(
        "{}{}{}",
        t.name,
        if t.args.is_empty() {
            "".into()
        } else {
            format!(
                "<{}>",
                t.args.iter().map(typ).collect::<Vec<_>>().join(", ")
            )
        },
        if t.optional { "?" } else { "" }
    )
}
fn params(p: &[Param], r: Option<&RestParam>) -> String {
    let mut v = p
        .iter()
        .map(|p| {
            format!(
                "{}{}{}",
                p.name,
                p.ty.as_ref()
                    .map(|t| format!(": {}", typ(t)))
                    .unwrap_or_default(),
                p.default
                    .as_ref()
                    .map(|e| format!(" = {}", expr(e)))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>();
    if let Some(r) = r {
        v.push(format!(
            "...{}{}",
            r.name,
            r.ty.as_ref()
                .map(|t| format!(": {}", typ(t)))
                .unwrap_or_default()
        ))
    }
    v.join(", ")
}
fn quote(s: &str) -> String {
    format!(
        "\"{}\"",
        s.chars().flat_map(char::escape_default).collect::<String>()
    )
}
/// Record keys re-quote when they are not identifier-shaped, so the printed
/// record re-parses as a record and not a block (D11).
fn record_key(name: &str) -> String {
    if crate::lexer::is_ident(name) {
        name.to_string()
    } else {
        quote(name)
    }
}
fn expr(e: &Expr) -> String {
    match e {
        Expr::Null { .. } => "null".into(),
        Expr::Bool { value, .. } => value.to_string(),
        Expr::Int { value, .. } => value.to_string(),
        Expr::Float { value, .. } => value.to_string(),
        Expr::Str { value, .. } => quote(value),
        Expr::StrInterp { parts, .. } => format!(
            "\"{}\"",
            parts
                .iter()
                .map(|p| match p {
                    StrPart::Lit { text } => text.replace('{', "\\{").replace('}', "\\}"),
                    StrPart::Expr { expr: e } => format!("{{{}}}", expr(e)),
                })
                .collect::<String>()
        ),
        Expr::Size { bytes, .. } => format!("{bytes}b"),
        Expr::Duration { ns, .. } => format!("{ns}ns"),
        Expr::Time { hour, min, sec, .. } => {
            if *sec == 0 {
                format!("{hour:02}:{min:02}")
            } else {
                format!("{hour:02}:{min:02}:{sec:02}")
            }
        }
        Expr::DateTime { iso, .. } => format!("t{}", quote(iso)),
        Expr::Regex { src, .. } => format!("re{}", quote(src)),
        Expr::Var { name, .. } => name.clone(),
        Expr::Field {
            recv,
            name,
            optional,
            ..
        } => format!(
            "{}{}{}",
            atom(recv),
            if *optional { "?." } else { "." },
            name
        ),
        Expr::Index { recv, index, .. } => format!("{}[{}]", atom(recv), expr(index)),
        Expr::MethodCall {
            recv,
            name,
            args,
            optional,
            ..
        } => format!(
            "{}{}{}({})",
            atom(recv),
            if *optional { "?." } else { "." },
            name,
            args_fmt(args)
        ),
        Expr::FnCall { name, args, .. } => format!("{name}({})", args_fmt(args)),
        Expr::Cmd { call, .. } => cmd(call),
        Expr::Lambda {
            params: p, body, ..
        } => format!("({}) => {}", params(p, None), lambda_body(body)),
        Expr::List { items, .. } => format!(
            "[{}]",
            items.iter().map(expr).collect::<Vec<_>>().join(", ")
        ),
        Expr::Record { fields, .. } => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|f| format!("{}: {}", record_key(&f.name), expr(&f.value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Expr::Block { block: b, .. } => block(b),
        Expr::If {
            cond, then, r#else, ..
        } => format!(
            "if {} {}{}",
            expr(cond),
            block(then),
            r#else
                .as_ref()
                .map(|x| format!(" else {}", expr(x)))
                .unwrap_or_default()
        ),
        Expr::Match {
            scrutinee, arms, ..
        } => format!(
            "match {} {{\n{}\n}}",
            expr(scrutinee),
            arms.iter()
                .map(|a| format!(
                    "  {}{} => {}",
                    a.patterns.iter().map(pat).collect::<Vec<_>>().join(" | "),
                    a.guard
                        .as_ref()
                        .map(|g| format!(" if {}", expr(g)))
                        .unwrap_or_default(),
                    expr(&a.body)
                ))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        Expr::Try {
            body,
            pattern,
            handler,
            ..
        } => format!(
            "try {} catch{} {}",
            block(body),
            pattern
                .as_ref()
                .map(|p| format!(" {}", pat(p)))
                .unwrap_or_default(),
            block(handler)
        ),
        Expr::Catch {
            expr: e,
            binder,
            handler,
            ..
        } => format!(
            "{} catch{} {}",
            expr(e),
            binder.as_ref().map(|b| format!(" {b}")).unwrap_or_default(),
            expr(handler)
        ),
        Expr::With {
            cwd,
            env,
            reef,
            body,
            ..
        } => format!(
            "with {} {}",
            [
                cwd.as_ref().map(|x| format!("cwd: {}", expr(x))),
                env.as_ref().map(|x| format!("env: {}", expr(x))),
                reef.as_ref().map(|x| format!("reef: {}", expr(x))),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", "),
            block(body)
        ),
        Expr::Spawn { body, .. } => format!("spawn {}", block(body)),
        Expr::LangBlock { tool, src, .. } => format!("{tool} {{ {src} }}"),
        Expr::Binary { op, lhs, rhs, .. } => format!("({} {} {})", expr(lhs), bop(*op), expr(rhs)),
        Expr::Unary { op, expr: e, .. } => {
            format!("{}{}", if *op == UnOp::Not { "!" } else { "-" }, atom(e))
        }
        Expr::Range {
            start,
            end,
            inclusive,
            ..
        } => format!(
            "{}{}{}",
            atom(start),
            if *inclusive { "..=" } else { ".." },
            atom(end)
        ),
    }
}

/// A brace immediately after `=>` is deliberately parsed as a closure block.
/// Parenthesize record-valued bodies so formatting never changes a record
/// projection into a block whose first field is dispatched as a command.
fn lambda_body(body: &Expr) -> String {
    match body {
        Expr::Record { .. } => format!("({})", expr(body)),
        _ => expr(body),
    }
}
fn atom(e: &Expr) -> String {
    match e {
        Expr::Var { .. }
        | Expr::Int { .. }
        | Expr::Float { .. }
        | Expr::Str { .. }
        | Expr::Bool { .. }
        | Expr::Null { .. }
        | Expr::List { .. }
        | Expr::Record { .. }
        | Expr::FnCall { .. }
        | Expr::MethodCall { .. }
        | Expr::Field { .. }
        | Expr::Index { .. } => expr(e),
        _ => format!("({})", expr(e)),
    }
}
fn bop(o: BinOp) -> &'static str {
    match o {
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Rem => "%",
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::In => "in",
        BinOp::And => "&&",
        BinOp::Or => "||",
        BinOp::Coalesce => "??",
    }
}
fn args_fmt(a: &Args) -> String {
    a.pos
        .iter()
        .map(expr)
        .chain(
            a.named
                .iter()
                .map(|n| format!("{}: {}", n.name, expr(&n.value))),
        )
        .collect::<Vec<_>>()
        .join(", ")
}
fn cmd(c: &CmdCall) -> String {
    let mut v = c
        .env_prefix
        .iter()
        .map(|e| format!("{}={}", e.name, carg(&e.value)))
        .collect::<Vec<_>>();
    v.push(format!("{}{}", if c.forced { "^" } else { "" }, c.head));
    v.extend(c.args.iter().map(carg));
    v.extend(c.redirects.iter().map(|r| {
        format!(
            "{} {}",
            match r.kind {
                RedirectKind::Out => ">",
                RedirectKind::Append => ">>",
                RedirectKind::In => "<",
            },
            carg(&r.target)
        )
    }));
    if c.background {
        v.push("&".into())
    }
    if let Some(b) = &c.trailing {
        v.push(block(b))
    }
    v.join(" ")
}
fn carg(a: &CmdArg) -> String {
    match a {
        CmdArg::Word { text, .. } | CmdArg::Path { text, .. } => text.clone(),
        CmdArg::Glob { pattern, .. } => pattern.clone(),
        CmdArg::Str { expr: e, .. } => expr(e),
        CmdArg::Expr { expr: e, .. } => format!("({})", expr(e)),
        CmdArg::FlagLong { name, value, .. } => format!(
            "--{}{}",
            name.replace('_', "-"),
            value
                .as_ref()
                .map(|v| format!("={}", carg(v)))
                .unwrap_or_default()
        ),
        CmdArg::FlagShort { chars, .. } => format!("-{chars}"),
        CmdArg::DashDash { .. } => "--".into(),
        CmdArg::Dash { .. } => "-".into(),
    }
}
fn pat(p: &Pattern) -> String {
    match p {
        Pattern::Wildcard { .. } => "_".into(),
        Pattern::Bind { name, .. } => name.clone(),
        Pattern::Lit { expr: e, .. } => expr(e),
        Pattern::Range {
            start,
            end,
            inclusive,
            ..
        } => format!(
            "{}{}{}",
            expr(start),
            if *inclusive { "..=" } else { ".." },
            expr(end)
        ),
        Pattern::Type { ty, name, .. } => format!(
            "{}{}",
            typ(ty),
            name.as_ref().map(|n| format!(" {n}")).unwrap_or_default()
        ),
        Pattern::Record { fields, .. } => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|f| f
                    .pattern
                    .as_ref()
                    .map(|p| format!("{}: {}", f.name, pat(p)))
                    .unwrap_or_else(|| f.name.clone()))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Pattern::List { items, rest, .. } => {
            let mut v = items.iter().map(pat).collect::<Vec<_>>();
            if let Some(r) = rest {
                v.push(format!("...{r}"))
            }
            format!("[{}]", v.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse;
    #[test]
    fn roundtrip_and_idempotent() {
        for src in [
            "let x = 2 + 3 * 4\nx - 1",
            "fn add(a: int, b: int = 1) { a + b }\nadd(2)",
            "git push --force > out &",
            "if true { [1, 2] } else { [] }",
            "[{name: \"api\", ready: true}].map(row => ({name: row.name, ready: row.ready}))",
            "[1, 2].reduce({count: 0}, (acc, n) => ({count: acc.count + n}))",
        ] {
            let a = parse(src).unwrap();
            let text = format_program(&a);
            let b = parse(&text).unwrap();
            assert!(canonical_equivalent(&a, &b), "{src} => {text}");
            assert_eq!(format_program(&b), text)
        }
    }
}
