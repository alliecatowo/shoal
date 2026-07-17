//! Exhaustive statement-dispatch matrix for site/content/internals/language-conformance-contract.md rules 1–4. Each row pins
//! the classification of the final statement's shape given the pre-seeded value
//! and command bindings.

use shoal_ast::*;
use shoal_syntax::{ParseCtx, parse_with_ctx};

fn ekind(e: &Expr) -> String {
    match e {
        Expr::Cmd { call, .. } => {
            format!("cmd:{}{}", if call.forced { "^" } else { "" }, call.head)
        }
        Expr::Binary { .. } => "expr:binary".into(),
        Expr::Unary { .. } => "expr:unary".into(),
        Expr::Range { .. } => "expr:range".into(),
        Expr::Var { .. } => "expr:var".into(),
        Expr::MethodCall { .. } => "expr:method".into(),
        Expr::FnCall { .. } => "expr:fncall".into(),
        Expr::Field { .. } => "expr:field".into(),
        Expr::Index { .. } => "expr:index".into(),
        Expr::List { .. } => "expr:list".into(),
        Expr::Record { .. } => "expr:record".into(),
        Expr::Str { .. } | Expr::StrInterp { .. } => "expr:str".into(),
        Expr::Int { .. } => "expr:int".into(),
        Expr::Float { .. } => "expr:float".into(),
        Expr::Bool { .. } => "expr:bool".into(),
        Expr::Null { .. } => "expr:null".into(),
        Expr::If { .. } => "expr:if".into(),
        Expr::Match { .. } => "expr:match".into(),
        Expr::LangBlock { .. } => "expr:sh".into(),
        Expr::Spawn { .. } => "expr:spawn".into(),
        Expr::Lambda { .. } => "expr:lambda".into(),
        other => panic!("unclassified expr {other:?}"),
    }
}
fn kind(s: &Stmt) -> String {
    match s {
        Stmt::Let { .. } => "let".into(),
        Stmt::Fn { .. } => "fn".into(),
        Stmt::Alias { .. } => "alias".into(),
        Stmt::Use { .. } => "use".into(),
        Stmt::Assign { .. } => "assign".into(),
        Stmt::Return { .. } => "return".into(),
        Stmt::Break { .. } => "break".into(),
        Stmt::Continue { .. } => "continue".into(),
        Stmt::For { .. } => "for".into(),
        Stmt::While { .. } => "while".into(),
        Stmt::Expr { expr, .. } => ekind(expr),
    }
}

struct Row {
    src: &'static str,
    value_bound: &'static [&'static str],
    cmd_bound: &'static [&'static str],
    repl: bool,
    want: &'static str,
}
const fn row(src: &'static str, want: &'static str) -> Row {
    Row {
        src,
        value_bound: &[],
        cmd_bound: &[],
        repl: false,
        want,
    }
}

#[test]
fn dispatch_matrix() {
    let rows = [
        // ---- Rule 1: reserved-word constructs ----
        row("let x = 1", "let"),
        row("var y = 2", "let"),
        row("export let z = 1", "let"),
        row("fn f(){1}", "fn"),
        row("alias a = ls", "alias"),
        row("use foo", "use"),
        row("return 1", "return"),
        row("break", "break"),
        row("continue", "continue"),
        row("for x in [1] { x }", "for"),
        row("while true { 1 }", "while"),
        // ---- Rule 2: non-identifier head → EXPR ----
        row("5 + 5", "expr:binary"),
        row("-3", "expr:unary"),
        row("!true", "expr:unary"),
        row("\"str\"", "expr:str"),
        row("1.5", "expr:float"),
        row("42", "expr:int"),
        row("true", "expr:bool"),
        row("null", "expr:null"),
        row("[1,2]", "expr:list"),
        row("{a:1}", "expr:record"),
        row("if true {1} else {2}", "expr:if"),
        row("match 1 { _ => 0 }", "expr:match"),
        row("sh { ls }", "expr:sh"),
        row("sh {|}", "expr:sh"),
        row("spawn {1}", "expr:spawn"),
        row("(echo hi)", "cmd:echo"), // group substitution → command
        // ---- Rule 3: identifier dispatch ----
        Row {
            src: "x = 5",
            value_bound: &["x"],
            want: "assign",
            cmd_bound: &[],
            repl: false,
        },
        Row {
            src: "x += 1",
            value_bound: &["x"],
            want: "assign",
            cmd_bound: &[],
            repl: false,
        },
        Row {
            src: "x - 1",
            value_bound: &["x"],
            want: "expr:binary",
            cmd_bound: &[],
            repl: false,
        },
        Row {
            src: "x",
            value_bound: &["x"],
            want: "expr:var",
            cmd_bound: &[],
            repl: false,
        },
        Row {
            src: "arr[0]",
            value_bound: &["arr"],
            want: "expr:index",
            cmd_bound: &[],
            repl: false,
        },
        Row {
            src: "obj.field",
            value_bound: &["obj"],
            want: "expr:field",
            cmd_bound: &[],
            repl: false,
        },
        row("ls", "cmd:ls"),                     // unbound → command
        row("cat nope.txt", "cmd:cat"),          // unbound with args
        row("FOO=bar ls", "cmd:ls"),             // env-prefix → command
        row("ls.where(.x==1)", "expr:method"),   // adjacency → EXPR
        row("run(\"a\", \"b\")", "expr:fncall"), // dynamic call form
        Row {
            src: "deploy staging --dry",
            value_bound: &[],
            cmd_bound: &["deploy"],
            repl: false,
            want: "cmd:deploy",
        },
        Row {
            src: "greet --loud",
            value_bound: &[],
            cmd_bound: &["greet"],
            repl: false,
            want: "cmd:greet",
        },
        Row {
            src: "gs -sb",
            value_bound: &[],
            cmd_bound: &["gs"],
            repl: false,
            want: "cmd:gs",
        },
        // ---- Rule 4: escape hatches / paths ----
        row("^ls", "cmd:^ls"),
        Row {
            src: "^ls", // forced even when value-shadowed
            value_bound: &["ls"],
            cmd_bound: &[],
            repl: false,
            want: "cmd:^ls",
        },
        row("^docker-compose up", "cmd:^docker-compose"),
        row("./x.sh", "cmd:./x.sh"),
        row("/bin/ls", "cmd:/bin/ls"),
        row("~/bin/foo", "cmd:~/bin/foo"),
    ];

    let mut failures = vec![];
    for r in &rows {
        let ctx = ParseCtx {
            repl: r.repl,
            value_bound: r.value_bound.iter().map(|s| s.to_string()).collect(),
            cmd_bound: r.cmd_bound.iter().map(|s| s.to_string()).collect(),
        };
        match parse_with_ctx(r.src, ctx) {
            Ok(p) => {
                let got = kind(p.stmts.last().expect("a statement"));
                if got != r.want {
                    failures.push(format!("{:?}: want {}, got {}", r.src, r.want, got));
                }
            }
            Err(e) => failures.push(format!("{:?}: parse error {}", r.src, e.msg)),
        }
    }
    assert!(rows.len() >= 40, "matrix must cover ≥40 cases");
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}
