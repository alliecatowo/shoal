//! Regression tests for the parser/lexer audit defects D1–D13 plus the
//! parser-side eval-audit items (#2 fn/alias two-scope, #6 `&&`/`||` command
//! operands, #7 `cmd &` lookahead) and the REPL leading-`.` chain. The stable
//! parser/formatter design record is `site/content/internals/parser-formatter.md`.

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

// ---------------------------------------------------------------- syntax-gaps:
// IIFE call restriction, match-guard-lambda mis-parse, `(it)`/`(out)` REPL bypass.
// (spec/cases/closures-more.toml, match-more.toml, edges.toml)

/// site/content/internals/language-conformance-contract.md `postfix = primary { … | call [trailing] }` grammar makes
/// `lambda` an ordinary `primary` with no carve-out against an immediate
/// `call` postfix — a parenthesized lambda literal must be directly
/// callable, not just callable once bound to a name.
#[test]
fn syntax_gap_iife_direct_call_of_lambda_literal() {
    let p = parse("(x => x + 1)(5)").unwrap();
    match last(&p) {
        Stmt::Expr {
            expr: Expr::Block { block, .. },
            ..
        } => {
            assert_eq!(block.stmts.len(), 2);
            assert!(matches!(
                block.stmts[0],
                Stmt::Let {
                    init: Expr::Lambda { .. },
                    ..
                }
            ));
            match &block.stmts[1] {
                Stmt::Expr {
                    expr: Expr::FnCall { args, .. },
                    ..
                } => assert!(matches!(args.pos[0], Expr::Int { value: 5, .. })),
                other => panic!("{other:?}"),
            }
        }
        other => panic!("{other:?}"),
    }
    // A named-function call still works exactly as before (no regression).
    assert!(matches!(
        last(&parse("let h = x => x + 1\nh(5)").unwrap()),
        Stmt::Expr {
            expr: Expr::FnCall { .. },
            ..
        }
    ));
    // Calling a fn-literal thunk directly also goes through the same path.
    assert!(parse("(() => 1)()").is_ok());
}

/// `primary()`'s bare `IDENT => …` one-param-lambda shorthand must only
/// fire at the head of a fresh expression, never as the rhs of a binary
/// operator — otherwise a guard's trailing bound name swallows the match
/// arm's own `=>` (reproduced minimally below).
#[test]
fn syntax_gap_match_guard_trailing_bound_name_does_not_swallow_arrow() {
    let p = parse("match 5 { n if flag => 1; _ => 0 }").unwrap();
    match last(&p) {
        Stmt::Expr {
            expr: Expr::Match { arms, .. },
            ..
        } => {
            assert_eq!(arms.len(), 2);
            assert!(matches!(arms[0].guard, Some(Expr::Var { .. })));
        }
        other => panic!("{other:?}"),
    }
    let p = parse(r#"match [1, 2] { [a, b] if a > b => "desc"; _ => "not-desc" }"#).unwrap();
    match last(&p) {
        Stmt::Expr {
            expr: Expr::Match { arms, .. },
            ..
        } => assert!(matches!(
            arms[0].guard,
            Some(Expr::Binary { op: BinOp::Gt, .. })
        )),
        other => panic!("{other:?}"),
    }
    // A call-argument lambda shorthand elsewhere in the guard is unaffected
    // (its own fresh `expr(0)`, not the guard's own leading operand).
    assert!(parse("match [1, 2, 3] { xs if xs.any(n => n > 2) => 1; _ => 0 }").is_ok());
}

/// `paren_or_lambda`'s single-bare-identifier tentative-lambda-params path
/// must fall back to the same `it`/`out` REPL-only check `primary()` uses,
/// not to unconditional command dispatch (which would treat `it`/`out` as
/// ordinary — if REPL-context-less — command names).
#[test]
fn syntax_gap_paren_it_out_rejected_in_scripts() {
    assert!(parse("(it)").is_err());
    assert!(parse("(out)").is_err());
    assert!(
        parse_with_ctx(
            "(it)",
            ParseCtx {
                repl: true,
                ..vb(&[])
            }
        )
        .is_ok()
    );
    // An ordinary single-bare-identifier group is unaffected: an unbound
    // name still dispatches as a command substitution (D4), and a bound
    // one still resolves to the value.
    assert!(matches!(
        last(&parse("(echo)").unwrap()),
        Stmt::Expr {
            expr: Expr::Cmd { .. },
            ..
        }
    ));
    assert!(matches!(
        last(&parse("let ls = 5\n(ls)").unwrap()),
        Stmt::Expr {
            expr: Expr::Var { .. },
            ..
        }
    ));
}

// ---------------------------------------------------------------- for-loop
// iterable vs. trailing-block-lambda ambiguity (dogfooding papercut).
//
// `postfix()`'s `f(a){…}` trailing-block desugar (site/content/internals/language-conformance-contract.md) greedily grabs a
// `{` right after a call's `)`. When that call is the *whole* for-loop
// iterable (`for p in glob("*.md") { … }`), the desugar swallowed the loop
// body's own brace as a bogus thunk argument to `glob(...)`, leaving the
// `for` with no `{` to open its body and a confusing "expected `{`" error.
// `for_stmt` now parses the iterable via `expr_before_block`, which
// suppresses that desugar at the iterable's top level while still allowing
// it on any nested, delimiter-enclosed subexpression.

fn for_iter(p: &Program) -> &Expr {
    match last(p) {
        Stmt::For { iter, .. } => iter,
        other => panic!("expected a for-loop, got {other:?}"),
    }
}

#[test]
fn for_in_over_a_call_parses() {
    let p = parse(r#"for p in glob("*.md") { n += 1 }"#).unwrap();
    assert!(matches!(for_iter(&p), Expr::FnCall { name, .. } if name == "glob"));
}

#[test]
fn for_in_over_a_method_chain_parses() {
    let p = parse(r#"for p in glob("*.md").filter(x => x.ok) { n += 1 }"#).unwrap();
    assert!(matches!(for_iter(&p), Expr::MethodCall { name, .. } if name == "filter"));
}

#[test]
fn for_in_over_a_range_parses() {
    let p = parse("for i in 1..10 { n += i }").unwrap();
    assert!(matches!(for_iter(&p), Expr::Range { .. }));
}

#[test]
fn for_in_over_a_var_parses() {
    let p = parse("var xs = [1, 2, 3]\nfor x in xs { n += x }").unwrap();
    assert!(matches!(for_iter(&p), Expr::Var { name, .. } if name == "xs"));
}

#[test]
fn for_in_over_a_call_still_allows_a_parenthesised_trailing_block() {
    // The escape hatch: a call whose OWN trailing-block-lambda is wanted as
    // (part of) the iterable still works as long as it's not left bare at
    // the iterable's top level — parens fully enclose the ambiguity away.
    let p = parse("for x in (retry(3) { attempt() }) { n += 1 }").unwrap();
    match last(&p) {
        Stmt::For { iter, .. } => match iter {
            Expr::FnCall { name, args, .. } => {
                assert_eq!(name, "retry");
                // `3` plus the desugared trailing-block thunk.
                assert_eq!(args.pos.len(), 2);
                assert!(matches!(args.pos[1], Expr::Lambda { .. }));
            }
            other => panic!("{other:?}"),
        },
        other => panic!("{other:?}"),
    }
}

#[test]
fn call_trailing_block_lambda_still_works_outside_a_for_iterable() {
    // The suppression is scoped to the for-loop's iterable only; an
    // ordinary statement-position call keeps taking its trailing block as
    // the `f(a){…}` sugar (site/content/internals/language-conformance-contract.md) intends.
    let p = parse("retry(3) { attempt() }").unwrap();
    match last(&p) {
        Stmt::Expr {
            expr: Expr::FnCall { name, args, .. },
            ..
        } => {
            assert_eq!(name, "retry");
            assert_eq!(args.pos.len(), 2);
            assert!(matches!(args.pos[1], Expr::Lambda { .. }));
        }
        other => panic!("{other:?}"),
    }
}
