//! Regression tests for the parser/lexer audit defects D1–D13 plus the
//! parser-side eval-audit items (#2 fn/alias two-scope, #6 `&&`/`||` command
//! operands, #7 `cmd &` lookahead) and the REPL leading-`.` chain. Each test
//! uses the exact repro from `scratch/explore-parser-audit.md`.

use shoal_ast::*;
use shoal_syntax::{ParseCtx, canonical_equivalent, format_program, parse, parse_with_ctx};

fn vb(names: &[&str]) -> ParseCtx {
    ParseCtx {
        repl: false,
        value_bound: names.iter().map(|s| s.to_string()).collect(),
        cmd_bound: vec![],
    }
}
fn last(p: &Program) -> &Stmt {
    p.stmts.last().expect("at least one statement")
}
fn cmd(s: &Stmt) -> &CmdCall {
    match s {
        Stmt::Expr {
            expr: Expr::Cmd { call, .. },
            ..
        } => call,
        other => panic!("expected command statement, got {other:?}"),
    }
}

// ---------------------------------------------------------------- D1
#[test]
fn d1_non_identifier_head_is_expr_not_cmd_probe() {
    for src in ["[1, 2, 3]", "[3,1,2].sort()", "[1,2,3].len()"] {
        let p = parse(src).unwrap_or_else(|e| panic!("{src}: {}", e.msg));
        assert!(matches!(
            last(&p),
            Stmt::Expr {
                expr: Expr::List { .. } | Expr::MethodCall { .. },
                ..
            }
        ));
    }
    // The bare-list case must be a list literal, never a command probe error.
    assert!(matches!(
        last(&parse("[1, 2, 3]").unwrap()),
        Stmt::Expr {
            expr: Expr::List { .. },
            ..
        }
    ));
}

// ---------------------------------------------------------------- D2 / eval-audit #2
#[test]
fn d2_fn_and_alias_names_dispatch_as_commands() {
    let p = parse("fn deploy(env: str, dry: bool = false) { env }\ndeploy staging --dry").unwrap();
    let c = cmd(last(&p));
    assert_eq!(c.head, "deploy");
    assert!(matches!(c.args[0], CmdArg::Word { .. }));
    assert!(matches!(c.args[1], CmdArg::FlagLong { .. }));

    let p = parse("fn greet(){1}\ngreet --loud").unwrap();
    assert_eq!(cmd(last(&p)).head, "greet");

    let p = parse("alias gs = git status\ngs -sb").unwrap();
    let c = cmd(last(&p));
    assert_eq!(c.head, "gs");
    assert!(matches!(c.args[0], CmdArg::FlagShort { .. }));
}

#[test]
fn d2_value_binding_still_wins_over_command_shadow() {
    // A `let` shadow makes the name dispatch EXPR (`x - 1` is subtraction).
    let p = parse_with_ctx("x - 1", vb(&["x"])).unwrap();
    assert!(matches!(
        last(&p),
        Stmt::Expr {
            expr: Expr::Binary { op: BinOp::Sub, .. },
            ..
        }
    ));
}

// ---------------------------------------------------------------- D3
#[test]
fn d3_ident_adjacency_forces_expr() {
    // Unbound command head immediately followed by `.` → invoke-then-chain with
    // `Var(name)` as the receiver.
    let p = parse("ls.where(.x==1)").unwrap();
    match last(&p) {
        Stmt::Expr {
            expr: Expr::MethodCall { recv, name, .. },
            ..
        } => {
            assert_eq!(name, "where");
            assert!(matches!(**recv, Expr::Var { ref name, .. } if name == "ls"));
        }
        other => panic!("{other:?}"),
    }
    // `run("name", args…)` — the dynamic call form.
    let p = parse("run(\"docker-compose\", \"up\")").unwrap();
    match last(&p) {
        Stmt::Expr {
            expr: Expr::FnCall { name, args, .. },
            ..
        } => {
            assert_eq!(name, "run");
            assert_eq!(args.pos.len(), 2);
        }
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------- D4
#[test]
fn d4_command_substitution_in_parens() {
    // value position
    let p = parse("let x = (echo hi)").unwrap();
    assert!(matches!(last(&p), Stmt::Expr { .. } | Stmt::Let { .. }));
    match last(&p) {
        Stmt::Let {
            init: Expr::Cmd { call, .. },
            ..
        } => assert_eq!(call.head, "echo"),
        other => panic!("{other:?}"),
    }
    // bare group
    assert_eq!(cmd(last(&parse("(echo hi)").unwrap())).head, "echo");
    // nested inside a command argument: `echo (ls)`
    let p = parse("echo (ls)").unwrap();
    let c = cmd(last(&p));
    assert_eq!(c.head, "echo");
    match &c.args[0] {
        CmdArg::Expr {
            expr: Expr::Cmd { call, .. },
            ..
        } => assert_eq!(call.head, "ls"),
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------- D5
#[test]
fn d5_newline_continuation() {
    // trailing operator
    assert!(parse("let x = 1 +\n2").is_ok());
    assert!(parse("true &&\nfalse").is_ok());
    // inside delimiters
    assert!(parse("[1,\n2]").is_ok());
    assert!(parse_with_ctx("f(1,\n2)", vb(&["f"])).is_ok());
    assert!(parse("{a: 1,\nb: 2}").is_ok());
    // leading-`.` on next line continues the postfix chain
    let p = parse_with_ctx("let foo=[1]\nfoo\n.first()", vb(&[])).unwrap();
    assert!(matches!(
        last(&p),
        Stmt::Expr {
            expr: Expr::MethodCall { .. },
            ..
        }
    ));
    // `catch` on the next line attaches to the initializer:
    // `let x = f()⏎catch { 0 }` ≡ `let x = (f() catch { 0 })`.
    let p = parse_with_ctx("let x = f()\ncatch { 0 }", vb(&["f"])).unwrap();
    assert!(matches!(
        last(&p),
        Stmt::Let {
            init: Expr::Catch { .. },
            ..
        }
    ));
    let p = parse_with_ctx("if a {1}\nelse {2}", vb(&["a"])).unwrap();
    match last(&p) {
        Stmt::Expr {
            expr: Expr::If { r#else, .. },
            ..
        } => assert!(r#else.is_some()),
        other => panic!("{other:?}"),
    }
}

#[test]
fn d5_blocks_stay_newline_sensitive() {
    // A block is not a delimiter that swallows newlines: two statements remain.
    let p = parse("fn f(){\n1\n2\n}").unwrap();
    match &p.stmts[0] {
        Stmt::Fn { decl } => assert_eq!(decl.body.stmts.len(), 2),
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------- D6
#[test]
fn d6_trailing_block_after_call_and_method() {
    let p = parse_with_ctx("let xs=[1]\nxs.each { echo }", vb(&[])).unwrap();
    match last(&p) {
        Stmt::Expr {
            expr: Expr::MethodCall { name, args, .. },
            ..
        } => {
            assert_eq!(name, "each");
            assert_eq!(args.pos.len(), 1);
            assert!(matches!(args.pos[0], Expr::Lambda { .. }));
        }
        other => panic!("{other:?}"),
    }
    // `f(a){…}` in expr position
    let p = parse_with_ctx("f(1) { 2 }", vb(&["f"])).unwrap();
    match last(&p) {
        Stmt::Expr {
            expr: Expr::FnCall { args, .. },
            ..
        } => {
            assert_eq!(args.pos.len(), 2);
            assert!(matches!(args.pos[1], Expr::Lambda { .. }));
        }
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------- D7
#[test]
fn d7_missing_terminator_is_rejected() {
    assert!(parse("let a = 1 let b = 2").is_err());
    let e = parse("let x = 1\nx foo").unwrap_err();
    assert!(e.msg.contains("between statements"));
    assert!(e.hint.as_deref().unwrap().contains("^x"));
}

#[test]
fn d7_command_logical_operators() {
    // `&&` / `||` between command statements (eval-audit #6)
    let p = parse("ls && ls").unwrap();
    assert!(matches!(
        last(&p),
        Stmt::Expr {
            expr: Expr::Binary { op: BinOp::And, .. },
            ..
        }
    ));
    match last(&p) {
        Stmt::Expr {
            expr: Expr::Binary { lhs, rhs, .. },
            ..
        } => {
            assert!(matches!(**lhs, Expr::Cmd { .. }));
            assert!(matches!(**rhs, Expr::Cmd { .. }));
        }
        _ => unreachable!(),
    }
    // `||` with a command fallback
    let p = parse("sh { exit 1 } || echo fallback").unwrap();
    assert!(matches!(
        last(&p),
        Stmt::Expr {
            expr: Expr::Binary { op: BinOp::Or, .. },
            ..
        }
    ));
}

#[test]
fn d7_background_command_without_args() {
    // eval-audit #7: `cmd &` with no positional args must not trip the
    // assignment lookahead on the cmd-only `&` token.
    let p = parse("ls &").unwrap();
    let c = cmd(last(&p));
    assert_eq!(c.head, "ls");
    assert!(c.background);
}

// ---------------------------------------------------------------- D8
#[test]
fn d8_sh_triple_raw_form() {
    let p = parse("sh''' echo hi '''").unwrap();
    assert!(matches!(
        last(&p),
        Stmt::Expr {
            expr: Expr::LangBlock { .. },
            ..
        }
    ));
    // single-quote raw form too
    assert!(matches!(
        last(&parse("sh'echo hi'").unwrap()),
        Stmt::Expr {
            expr: Expr::LangBlock { .. },
            ..
        }
    ));
}

// ---------------------------------------------------------------- D9
#[test]
fn d9_it_out_rejected_in_scripts() {
    assert!(parse("let x = it").is_err());
    assert!(parse("let x = out[0]").is_err());
    // …but allowed in REPL context.
    assert!(
        parse_with_ctx(
            "let x = it",
            ParseCtx {
                repl: true,
                ..vb(&[])
            }
        )
        .is_ok()
    );
    assert!(
        parse_with_ctx(
            "let x = out[0]",
            ParseCtx {
                repl: true,
                ..vb(&[])
            }
        )
        .is_ok()
    );
}

// ---------------------------------------------------------------- D10
#[test]
fn d10_leading_glob_character_class() {
    let p = parse("ls [abc].txt").unwrap();
    let c = cmd(last(&p));
    assert_eq!(c.head, "ls");
    assert!(matches!(c.args[0], CmdArg::Glob { .. }));
    // A lone unclosed `[` keeps the teaching error.
    assert!(parse("ls [abc").is_err());
}

// ---------------------------------------------------------------- D11
#[test]
fn d11_formatter_requotes_non_ident_record_keys() {
    let a = parse("let r = {\"a-b\": 1}").unwrap();
    let text = format_program(&a);
    assert!(text.contains("\"a-b\""), "got {text}");
    let b = parse(&text).unwrap();
    assert!(canonical_equivalent(&a, &b), "{text}");
}

// ---------------------------------------------------------------- D12
#[test]
fn d12_uppercase_units_rejected() {
    let e = parse("1KB").unwrap_err();
    assert!(e.msg.contains("lowercase"));
    // lowercase still works
    assert!(parse("1kb").is_ok());
}

// ---------------------------------------------------------------- D13
#[test]
fn d13_leading_pipe_in_match_arm() {
    let e = parse("match x {\n | 0 => \"a\"\n}").unwrap_err();
    assert!(e.msg.contains('|'));
    assert!(e.hint.as_deref().unwrap().contains("alternation"));
}

// ---------------------------------------------------------------- REPL leading-`.`
#[test]
fn repl_leading_dot_chains_on_it() {
    let p = parse_with_ctx(
        ".first()",
        ParseCtx {
            repl: true,
            ..vb(&[])
        },
    )
    .unwrap();
    match last(&p) {
        Stmt::Expr {
            expr: Expr::MethodCall { recv, name, .. },
            ..
        } => {
            assert_eq!(name, "first");
            assert!(matches!(**recv, Expr::Var { ref name, .. } if name == "it"));
        }
        other => panic!("{other:?}"),
    }
    // In script context a leading `.` is not an `it` chain.
    assert!(parse(".first()").is_err());
}
