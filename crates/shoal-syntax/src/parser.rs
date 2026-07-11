use crate::lexer::{LexError, Lexer, Mode, RESERVED, Seg, Tok};
use shoal_ast::*;
use std::collections::HashSet;

mod block;
mod command;
mod expr;
mod pattern;
mod stmt;

/// Interpreter-class tools (IO.md §2.2): a head in this set, immediately
/// followed by `{` (or the triple-raw `'''`/`'` form), lexes a raw balanced-
/// brace block and produces `Expr::LangBlock` — the parse-time trigger. A head
/// *not* in this set keeps `{` as a trailing block/thunk (TDD §13.14). This is
/// the static parser-side gate; the eval side maps each tool to its inline-eval
/// invocation (shoal-eval `expr.rs`).
pub const INTERPRETERS: &[&str] = &[
    "sh",
    "bash",
    "python",
    "python3",
    "node",
    "deno",
    "ruby",
    "jq",
    "perl",
    "php",
    "lua",
    "Rscript",
    "osascript",
    "fish",
    "zsh",
];

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub msg: String,
    pub span: Span,
    pub hint: Option<String>,
}
impl ParseError {
    fn new(msg: impl Into<String>, span: Span) -> Self {
        Self {
            msg: msg.into(),
            span,
            hint: None,
        }
    }
    fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}
impl From<LexError> for ParseError {
    fn from(e: LexError) -> Self {
        Self {
            msg: e.msg,
            span: e.span,
            hint: e.hint,
        }
    }
}
impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.msg)
    }
}
impl std::error::Error for ParseError {}
pub type ParseResult<T> = Result<T, ParseError>;

/// Parse context carrying the dispatch inputs the parser cannot infer from the
/// source alone (TDD §3.1): whether we are at a REPL prompt (so `it`/`out` are
/// legal and a leading `.` chains on `it`), plus the pre-seeded value-bindings
/// (`let`/`var`/params) and command-bindings (session `fn`s/aliases).
#[derive(Debug, Clone, Default)]
pub struct ParseCtx {
    pub repl: bool,
    pub value_bound: Vec<String>,
    pub cmd_bound: Vec<String>,
}

pub fn parse(src: &str) -> ParseResult<Program> {
    Parser::new(src).parse_program()
}

pub fn parse_with_scope(
    src: &str,
    bound: impl IntoIterator<Item = String>,
) -> ParseResult<Program> {
    let mut parser = Parser::new(src);
    for name in bound {
        parser.bind(name);
    }
    parser.parse_program()
}

/// The full-fidelity entry point (TDD §3.1): dispatch honours the two scope
/// categories and the REPL context. `parse`/`parse_with_scope` are compat shims
/// mapping onto this with `repl: false` and no command bindings.
pub fn parse_with_ctx(src: &str, ctx: ParseCtx) -> ParseResult<Program> {
    let mut parser = Parser::new(src);
    parser.repl = ctx.repl;
    for name in ctx.value_bound {
        parser.bind(name);
    }
    for name in ctx.cmd_bound {
        parser.bind_cmd(name);
    }
    parser.parse_program()
}

pub struct Parser<'s> {
    lx: Lexer<'s>,
    pos: usize,
    /// Value bindings — `let`/`var`/params. A name here dispatches EXPR.
    scopes: Vec<HashSet<String>>,
    /// Command bindings — user `fn`s and aliases. A name here (and not also a
    /// value binding) dispatches CMD.
    cmd_scopes: Vec<HashSet<String>>,
    /// REPL context: `it`/`out` are legal and a leading `.` chains on `it`.
    repl: bool,
    /// Suppresses the `f(a){…}` trailing-block-lambda desugar (§3.4) at the
    /// *top level* of the expression currently being parsed. Set while
    /// parsing an expression that is immediately followed by a mandatory
    /// `{ … }` block belonging to an *enclosing* construct (e.g. a `for`
    /// loop's iterable) — without it, a bare call ending the expression
    /// (`for p in glob("*.md") { … }`) would have that `{` misparsed as its
    /// own trailing-block argument, starving the loop body of its brace.
    /// Cleared while parsing any subexpression fully enclosed by its own
    /// matching delimiter (call args, `[…]`, parenthesised groups), where a
    /// trailing block can never be confused for the outer construct's block.
    no_trailing_block: bool,
}

impl<'s> Parser<'s> {
    pub fn new(src: &'s str) -> Self {
        let builtins = ["path", "glob", "regex"]
            .into_iter()
            .map(str::to_string)
            .collect();
        Self {
            lx: Lexer::new(src),
            pos: 0,
            scopes: vec![builtins],
            cmd_scopes: vec![HashSet::new()],
            repl: false,
            no_trailing_block: false,
        }
    }
    pub(crate) fn peek(&self, m: Mode) -> ParseResult<(Tok, Span)> {
        Ok(self.lx.token(self.pos, m)?)
    }
    pub(crate) fn bump(&mut self, m: Mode) -> ParseResult<(Tok, Span)> {
        let x = self.peek(m)?;
        self.pos = x.1.end as usize;
        Ok(x)
    }
    pub(crate) fn eat(&mut self, m: Mode, want: &Tok) -> ParseResult<Option<Span>> {
        let (t, s) = self.peek(m)?;
        if std::mem::discriminant(&t) == std::mem::discriminant(want) {
            self.pos = s.end as usize;
            Ok(Some(s))
        } else {
            Ok(None)
        }
    }
    pub(crate) fn expect(&mut self, m: Mode, want: Tok, text: &str) -> ParseResult<Span> {
        self.eat(m, &want)?.ok_or_else(|| {
            let (_, s) = self
                .peek(m)
                .unwrap_or((Tok::Eof, Span::new(self.pos, self.pos)));
            ParseError::new(format!("expected {text}"), s)
        })
    }
    pub(crate) fn term(&mut self) -> ParseResult<()> {
        // Non-fatal: a head that lex-errors in EXPR mode (e.g. a `~/…` path
        // command) is not a terminator, so stop and let `statement()` dispatch.
        while let Ok((Tok::Newline | Tok::Semi, _)) = self.peek(Mode::Expr) {
            self.bump(Mode::Expr)?;
        }
        Ok(())
    }
    /// Peek the next EXPR token and test it; a lex error counts as "no match"
    /// so a CMD-only next head never aborts a statement loop's guard.
    pub(crate) fn peek_is(&self, f: impl Fn(&Tok) -> bool) -> bool {
        matches!(self.peek(Mode::Expr), Ok((t, _)) if f(&t))
    }
    pub(crate) fn bound(&self, n: &str) -> bool {
        self.scopes.iter().rev().any(|s| s.contains(n))
    }
    pub(crate) fn bind(&mut self, n: String) {
        self.scopes.last_mut().unwrap().insert(n);
    }
    pub(crate) fn bind_cmd(&mut self, n: String) {
        self.cmd_scopes.last_mut().unwrap().insert(n);
    }
    pub(crate) fn byte(&self, i: usize) -> u8 {
        self.lx.src.as_bytes().get(i).copied().unwrap_or(0)
    }
    /// Does the raw text at `start` begin a path literal (`./ ../ ~ ~/ /…`)?
    /// Such a head dispatches CMD (TDD §2.2 / §3.1 rule 2 is for EXPR starters,
    /// path words are command heads).
    pub(crate) fn is_path_head(&self, start: usize) -> bool {
        match self.byte(start) {
            b'/' => true,
            b'~' => {
                let n = self.byte(start + 1);
                n == b'/' || matches!(n, 0 | b' ' | b'\t' | b'\r' | b'\n' | b';')
            }
            b'.' => {
                let n = self.byte(start + 1);
                n == b'/' || (n == b'.' && self.byte(start + 2) == b'/')
            }
            _ => false,
        }
    }
    /// True when an interpreter-class head (`INTERPRETERS`) at `ident_span` is
    /// followed by a raw block: either an immediately-adjacent `'` (the
    /// `tool'''…'''` / `tool'…'` raw form, matching `sh`'s legacy spelling) or a
    /// `{` open brace (whitespace permitted, as `sh { … }` allows). This is the
    /// IO.md §2.3 parse-time trigger — checked *before* the brace is consumed so
    /// the parser knows to switch the lexer into raw mode.
    pub(crate) fn interp_block_follows(&self, ident_span: Span) -> bool {
        let end = ident_span.end as usize;
        if self.byte(end) == b'\'' {
            return true;
        }
        matches!(self.lx.token(end, Mode::Expr), Ok((Tok::LBrace, _)))
    }
    /// True when the token immediately after an identifier abuts it (no
    /// whitespace) and is a postfix opener `.`/`?.`/`(`/`[` — the §3.1
    /// ident-adjacency refinement forcing an EXPR statement.
    pub(crate) fn adjacent_postfix_after_ident(&self, ident_span: Span) -> ParseResult<bool> {
        Ok(match self.lx.token(ident_span.end as usize, Mode::Expr) {
            Ok((t, s)) => {
                s.start == ident_span.end
                    && matches!(t, Tok::Dot | Tok::QuestionDot | Tok::LParen | Tok::LBracket)
            }
            Err(_) => false,
        })
    }
    /// Consume a run of `Newline` tokens (delimiter-interior continuation, §2.1).
    pub(crate) fn skip_newlines(&mut self) -> ParseResult<()> {
        while matches!(self.peek(Mode::Expr)?.0, Tok::Newline) {
            self.bump(Mode::Expr)?;
        }
        Ok(())
    }
    /// Look past a run of newlines; if the next significant token satisfies
    /// `pred`, advance to just before it and return true (leading-`.`/`catch`/
    /// `else` cross-newline continuation, §2.1). Otherwise leave `pos` intact.
    pub(crate) fn continue_if<F: Fn(&Tok) -> bool>(&mut self, pred: F) -> ParseResult<bool> {
        let mut p = self.pos;
        loop {
            let (t, s) = match self.lx.token(p, Mode::Expr) {
                Ok(x) => x,
                Err(_) => return Ok(false),
            };
            match t {
                Tok::Newline => p = s.end as usize,
                _ if pred(&t) => {
                    self.pos = p;
                    return Ok(true);
                }
                _ => return Ok(false),
            }
        }
    }

    pub fn parse_program(mut self) -> ParseResult<Program> {
        let mut stmts = vec![];
        self.term()?;
        while !self.peek_is(|t| matches!(t, Tok::Eof)) {
            let stmt = self.statement()?;
            self.require_term(&stmt)?;
            stmts.push(stmt);
        }
        Ok(Program { stmts })
    }

    pub(crate) fn at_end_stmt(&self) -> ParseResult<bool> {
        Ok(matches!(
            self.peek(Mode::Expr)?.0,
            Tok::Newline | Tok::Semi | Tok::Eof | Tok::RBrace
        ))
    }
    pub(crate) fn ident(&mut self) -> ParseResult<(String, Span)> {
        match self.bump(Mode::Expr)? {
            (Tok::Ident(n), s) => Ok((n, s)),
            (_, s) => Err(ParseError::new("expected identifier", s)),
        }
    }
    pub(crate) fn pattern_bind(&mut self) -> ParseResult<Pattern> {
        let (n, s) = self.ident()?;
        Ok(if n == "_" {
            Pattern::Wildcard { span: s }
        } else {
            Pattern::Bind { name: n, span: s }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn arithmetic_precedence() {
        let p = parse("let x = 2 + 3 * 4\nx - 1").unwrap();
        assert_eq!(p.stmts.len(), 2);
        match &p.stmts[0] {
            Stmt::Let {
                init:
                    Expr::Binary {
                        op: BinOp::Add,
                        rhs,
                        ..
                    },
                ..
            } => assert!(matches!(**rhs, Expr::Binary { op: BinOp::Mul, .. })),
            x => panic!("{x:?}"),
        }
    }
    #[test]
    fn command_shape() {
        let p = parse("FOO=x git push *.rs --force > out &").unwrap();
        match &p.stmts[0] {
            Stmt::Expr {
                expr: Expr::Cmd { call, .. },
                ..
            } => {
                assert_eq!(call.head, "git");
                assert_eq!(call.env_prefix.len(), 1);
                assert!(call.background);
                assert_eq!(call.redirects.len(), 1)
            }
            x => panic!("{x:?}"),
        }
    }
    #[test]
    fn declarations_and_fn() {
        let p = parse("fn add(a: int, b: int = 1) -> int { a + b }\nlet z = add(2)").unwrap();
        assert!(matches!(p.stmts[0], Stmt::Fn { .. }));
    }
    #[test]
    fn teaching_pipe_error() {
        let e = parse("ls | wc").unwrap_err();
        assert!(e.msg.contains("no pipe operator"));
    }
    #[test]
    fn records_lists_and_chain() {
        let p = parse("let xs = [{name: \"a\"}]\nxs.where(.name == \"a\")").unwrap();
        match &p.stmts[1] {
            Stmt::Expr {
                expr: Expr::MethodCall { args, .. },
                ..
            } => assert!(matches!(args.pos[0], Expr::Lambda { .. })),
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn logical_operators_and_precedence() {
        for src in ["false && missing", "true || missing", "null ?? 3"] {
            assert!(parse(src).is_ok(), "failed to parse {src}");
        }
        let p = parse("true || false && null ?? 3").unwrap();
        match &p.stmts[0] {
            Stmt::Expr {
                expr:
                    Expr::Binary {
                        op: BinOp::Coalesce,
                        lhs,
                        ..
                    },
                ..
            } => assert!(matches!(**lhs, Expr::Binary { op: BinOp::Or, .. })),
            other => panic!("{other:?}"),
        }
    }
    #[test]
    fn remaining_expression_forms() {
        let p = parse("match 1 { 0 | 1 => \"bit\"\n _ => \"other\" }").unwrap();
        assert!(matches!(
            p.stmts[0],
            Stmt::Expr {
                expr: Expr::Match { .. },
                ..
            }
        ));
        let p = parse("with cwd: \"/tmp\", env: {A: \"b\"} { 1 }").unwrap();
        assert!(matches!(
            p.stmts[0],
            Stmt::Expr {
                expr: Expr::With { .. },
                ..
            }
        ));
        let p = parse("let f = (a: int, b: int) => a + b\nf(1, 2) catch { 0 }").unwrap();
        assert!(matches!(
            p.stmts[0],
            Stmt::Let {
                init: Expr::Lambda { .. },
                ..
            }
        ));
        assert!(matches!(
            p.stmts[1],
            Stmt::Expr {
                expr: Expr::Catch { .. },
                ..
            }
        ));
    }
}
