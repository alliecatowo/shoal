//! Canonical AST node definitions. See `docs/TDD.md` §3 for the grammar and
//! §3.4 for the desugaring rules that produce these nodes.

use crate::span::Span;
use serde::{Deserialize, Serialize};

/// A parsed source unit: a script file, a REPL line, or a `-c` string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Program {
    pub stmts: Vec<Stmt>,
}

// ---------------------------------------------------------------------------
// Statements
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Stmt {
    /// `let p = e` / `var p = e` / `export let p = e`
    Let {
        pattern: Pattern,
        ty: Option<Type>,
        init: Expr,
        mutable: bool,
        exported: bool,
        span: Span,
    },
    /// `fn name(params) -> ty { body }`
    Fn {
        decl: FnDecl,
    },
    /// `alias gs = git status` — AST-level partial application (TDD §1.8).
    Alias {
        name: String,
        target: CmdCall,
        span: Span,
    },
    /// `use ./lib/deploy`
    Use {
        path: String,
        span: Span,
    },
    /// `x = e`, `x += e`, `rec.field = e`, `xs[0] = e`
    Assign {
        target: Expr,
        op: AssignOp,
        value: Expr,
        span: Span,
    },
    Return {
        value: Option<Expr>,
        span: Span,
    },
    Break {
        span: Span,
    },
    Continue {
        span: Span,
    },
    For {
        pattern: Pattern,
        iter: Expr,
        body: Block,
        span: Span,
    },
    While {
        cond: Expr,
        body: Block,
        span: Span,
    },
    /// Everything else, including command statements (root `Expr::Cmd`).
    /// Statement-position semantics (PTY passthrough, `it` binding, raise on
    /// non-ok) key off this being a top-level statement — see TDD §1.2, §4.5.
    Expr {
        expr: Expr,
        span: Span,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssignOp {
    Set,
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FnDecl {
    pub name: String,
    pub params: Vec<Param>,
    /// `...rest` variadic tail.
    pub rest: Option<RestParam>,
    pub ret: Option<Type>,
    pub body: Block,
    /// Doc comment (`# ...` lines immediately above), for `--help` synthesis.
    pub doc: Option<String>,
    pub exported: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    pub ty: Option<Type>,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RestParam {
    pub name: String,
    pub ty: Option<Type>,
}

/// Type annotation: `str`, `int?`, `list<path>`, …
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Type {
    pub name: String,
    pub args: Vec<Type>,
    pub optional: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

// ---------------------------------------------------------------------------
// Expressions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Expr {
    Null {
        span: Span,
    },
    Bool {
        value: bool,
        span: Span,
    },
    Int {
        value: i64,
        span: Span,
    },
    Float {
        value: f64,
        span: Span,
    },
    /// Fully-resolved string literal (raw, or escaped with no interpolation).
    Str {
        value: String,
        span: Span,
    },
    /// `"text {expr} more"` — interpolating string.
    StrInterp {
        parts: Vec<StrPart>,
        span: Span,
    },
    /// `1.5gb` → bytes.
    Size {
        bytes: u64,
        span: Span,
    },
    /// `250ms` → nanoseconds.
    Duration {
        ns: i64,
        span: Span,
    },
    /// `10:00am`, `23:15`.
    Time {
        hour: u8,
        min: u8,
        sec: u8,
        span: Span,
    },
    /// `t"2026-07-09T14:00Z"` — parsed/validated at eval.
    DateTime {
        iso: String,
        span: Span,
    },
    /// `re"…"` — compiled at eval.
    Regex {
        src: String,
        span: Span,
    },
    Var {
        name: String,
        span: Span,
    },
    /// `recv.name` / `recv?.name`
    Field {
        recv: Box<Expr>,
        name: String,
        optional: bool,
        span: Span,
    },
    /// `recv[index]`
    Index {
        recv: Box<Expr>,
        index: Box<Expr>,
        span: Span,
    },
    /// `recv.name(args)` / `recv?.name(args)`
    MethodCall {
        recv: Box<Expr>,
        name: String,
        args: Args,
        optional: bool,
        span: Span,
    },
    /// `name(args)` — user fn, builtin fn, or closure-in-variable call.
    FnCall {
        name: String,
        args: Args,
        span: Span,
    },
    /// External/builtin command call, any position (TDD §1.2 position rule
    /// is decided by the evaluator from context, not stored here).
    Cmd {
        call: Box<CmdCall>,
        span: Span,
    },
    /// `x => e` / `(a, b: int) => { … }`
    Lambda {
        params: Vec<Param>,
        body: Box<Expr>,
        span: Span,
    },
    List {
        items: Vec<Expr>,
        span: Span,
    },
    Record {
        fields: Vec<RecordField>,
        span: Span,
    },
    /// `{ stmts; trailing-expr }` in expression position.
    Block {
        block: Block,
        span: Span,
    },
    If {
        cond: Box<Expr>,
        then: Block,
        r#else: Option<Box<Expr>>,
        span: Span,
    },
    Match {
        scrutinee: Box<Expr>,
        arms: Vec<MatchArm>,
        span: Span,
    },
    /// `try { … } catch p { … }`
    Try {
        body: Block,
        pattern: Option<Pattern>,
        handler: Block,
        span: Span,
    },
    /// Postfix `e catch h` / `e catch err h` (sugar for Try; kept distinct for
    /// lossless formatting).
    Catch {
        expr: Box<Expr>,
        binder: Option<String>,
        handler: Box<Expr>,
        span: Span,
    },
    /// `with cwd: p, env: {…}, reef: {…} { body }`
    With {
        cwd: Option<Box<Expr>>,
        env: Option<Box<Expr>>,
        /// `reef: {tool: constraint, …}` — dynamic reef scoping (REEF.md §6).
        /// Additive: existing `With` nodes parse with `reef: None`.
        reef: Option<Box<Expr>>,
        body: Block,
        span: Span,
    },
    /// `spawn { … }` → task.
    Spawn {
        body: Block,
        span: Span,
    },
    /// Interpreter block (IO.md §2): `tool { verbatim source }` /
    /// `tool ''' … '''`. `sh { … }` is just `tool: "sh"`. `tool` is resolved as
    /// a command at eval time; `src` is handed to it as its program.
    LangBlock {
        tool: String,
        src: String,
        span: Span,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        span: Span,
    },
    Unary {
        op: UnOp,
        expr: Box<Expr>,
        span: Span,
    },
    /// `a..b` / `a..=b`
    Range {
        start: Box<Expr>,
        end: Box<Expr>,
        inclusive: bool,
        span: Span,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StrPart {
    Lit { text: String },
    Expr { expr: Expr },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordField {
    pub name: String,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Args {
    pub pos: Vec<Expr>,
    pub named: Vec<NamedArg>,
}

impl Args {
    pub fn empty() -> Self {
        Args {
            pos: Vec::new(),
            named: Vec::new(),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.pos.is_empty() && self.named.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedArg {
    pub name: String,
    pub value: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MatchArm {
    pub patterns: Vec<Pattern>,
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    And,
    Or,
    Coalesce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnOp {
    Not,
    Neg,
}

// ---------------------------------------------------------------------------
// Patterns
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Pattern {
    Wildcard {
        span: Span,
    },
    Bind {
        name: String,
        span: Span,
    },
    /// Literal pattern (restricted to literal `Expr` kinds, incl. negatives).
    Lit {
        expr: Box<Expr>,
        span: Span,
    },
    Range {
        start: Box<Expr>,
        end: Box<Expr>,
        inclusive: bool,
        span: Span,
    },
    /// `int n` / `str s` — type test + bind.
    Type {
        ty: Type,
        name: Option<String>,
        span: Span,
    },
    /// `{name, size: s}` — field destructure; `None` sub-pattern binds the field name.
    Record {
        fields: Vec<FieldPat>,
        span: Span,
    },
    /// `[a, b, ...rest]`
    List {
        items: Vec<Pattern>,
        rest: Option<String>,
        span: Span,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldPat {
    pub name: String,
    pub pattern: Option<Pattern>,
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// A command call: head word + CMD-mode arguments (TDD §2.2, §3.2 `command`).
/// Flags remain structured here; they are resolved against the callee's
/// signature (fn params or adapter schema) at bind time (TDD §4.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CmdCall {
    pub head: String,
    /// `^head` — forced command interpretation past shadowing.
    pub forced: bool,
    pub args: Vec<CmdArg>,
    pub redirects: Vec<Redirect>,
    /// Leading `NAME=value` prefixes (desugar: `with env: {…} { … }`).
    pub env_prefix: Vec<EnvPrefix>,
    /// Trailing `&` (desugar: `spawn { … }`).
    pub background: bool,
    /// Trailing `{ … }` block argument (thunk).
    pub trailing: Option<Block>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnvPrefix {
    pub name: String,
    pub value: CmdArg,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CmdArg {
    /// Bare word → `str` (coerced at bind time per the callee signature).
    Word { text: String, span: Span },
    /// Path literal (`./x`, `~/x`, `/x`, `../x`) — unexpanded source text.
    Path { text: String, span: Span },
    /// Glob literal — expansion site is the callee (TDD §4.3).
    Glob { pattern: String, span: Span },
    /// Quoted string argument (may interpolate): `Expr::Str` or `Expr::StrInterp`.
    Str { expr: Expr, span: Span },
    /// `(expr)` embedded expression argument.
    Expr { expr: Expr, span: Span },
    /// `--name` / `--name=value`. Space-separated values arrive as a
    /// following positional arg and are merged at bind time.
    FlagLong {
        name: String,
        value: Option<Box<CmdArg>>,
        span: Span,
    },
    /// `-abc` — exploded per adapter short-flag table at bind time.
    FlagShort { chars: String, span: Span },
    /// `--` end-of-flags marker.
    DashDash { span: Span },
    /// Bare `-` (stdin convention; passed through verbatim, TDD §13.3).
    Dash { span: Span },
}

impl CmdArg {
    pub fn span(&self) -> Span {
        match self {
            CmdArg::Word { span, .. }
            | CmdArg::Path { span, .. }
            | CmdArg::Glob { span, .. }
            | CmdArg::Str { span, .. }
            | CmdArg::Expr { span, .. }
            | CmdArg::FlagLong { span, .. }
            | CmdArg::FlagShort { span, .. }
            | CmdArg::DashDash { span }
            | CmdArg::Dash { span } => *span,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RedirectKind {
    /// `> file` — `.save(file)` on stdout bytes.
    Out,
    /// `>> file` — `.append(file)`.
    Append,
    /// `< file` — stdin from file.
    In,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Redirect {
    pub kind: RedirectKind,
    pub target: CmdArg,
    pub span: Span,
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Null { span }
            | Expr::Bool { span, .. }
            | Expr::Int { span, .. }
            | Expr::Float { span, .. }
            | Expr::Str { span, .. }
            | Expr::StrInterp { span, .. }
            | Expr::Size { span, .. }
            | Expr::Duration { span, .. }
            | Expr::Time { span, .. }
            | Expr::DateTime { span, .. }
            | Expr::Regex { span, .. }
            | Expr::Var { span, .. }
            | Expr::Field { span, .. }
            | Expr::Index { span, .. }
            | Expr::MethodCall { span, .. }
            | Expr::FnCall { span, .. }
            | Expr::Cmd { span, .. }
            | Expr::Lambda { span, .. }
            | Expr::List { span, .. }
            | Expr::Record { span, .. }
            | Expr::Block { span, .. }
            | Expr::If { span, .. }
            | Expr::Match { span, .. }
            | Expr::Try { span, .. }
            | Expr::Catch { span, .. }
            | Expr::With { span, .. }
            | Expr::Spawn { span, .. }
            | Expr::LangBlock { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Range { span, .. } => *span,
        }
    }
}

impl Stmt {
    pub fn span(&self) -> Span {
        match self {
            Stmt::Let { span, .. }
            | Stmt::Alias { span, .. }
            | Stmt::Use { span, .. }
            | Stmt::Assign { span, .. }
            | Stmt::Return { span, .. }
            | Stmt::Break { span }
            | Stmt::Continue { span }
            | Stmt::For { span, .. }
            | Stmt::While { span, .. }
            | Stmt::Expr { span, .. } => *span,
            Stmt::Fn { decl } => decl.span,
        }
    }
}

impl Pattern {
    pub fn span(&self) -> Span {
        match self {
            Pattern::Wildcard { span }
            | Pattern::Bind { span, .. }
            | Pattern::Lit { span, .. }
            | Pattern::Range { span, .. }
            | Pattern::Type { span, .. }
            | Pattern::Record { span, .. }
            | Pattern::List { span, .. } => *span,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ast_json_roundtrip() {
        let prog = Program {
            stmts: vec![Stmt::Let {
                pattern: Pattern::Bind {
                    name: "x".into(),
                    span: Span::new(4, 5),
                },
                ty: None,
                init: Expr::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(Expr::Int {
                        value: 2,
                        span: Span::new(8, 9),
                    }),
                    rhs: Box::new(Expr::Size {
                        bytes: 1500,
                        span: Span::new(12, 17),
                    }),
                    span: Span::new(8, 17),
                },
                mutable: false,
                exported: false,
                span: Span::new(0, 17),
            }],
        };
        let json = serde_json::to_string(&prog).unwrap();
        assert!(json.contains("\"kind\":\"let\""));
        let back: Program = serde_json::from_str(&json).unwrap();
        assert_eq!(prog, back);
    }

    #[test]
    fn cmd_call_roundtrip() {
        let call = CmdCall {
            head: "git".into(),
            forced: false,
            args: vec![
                CmdArg::Word {
                    text: "push".into(),
                    span: Span::new(4, 8),
                },
                CmdArg::FlagLong {
                    name: "force".into(),
                    value: None,
                    span: Span::new(9, 16),
                },
            ],
            redirects: vec![],
            env_prefix: vec![],
            background: false,
            trailing: None,
            span: Span::new(0, 16),
        };
        let json = serde_json::to_string(&call).unwrap();
        let back: CmdCall = serde_json::from_str(&json).unwrap();
        assert_eq!(call, back);
    }
}
