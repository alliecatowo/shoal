//! The modal lexer (site/content/internals/language-conformance-contract.md). The parser drives the lexer between `CMD` mode
//! (command word soup) and `EXPR` mode (conventional tokens); mode is a
//! property of grammar position, never runtime state. The lexer is
//! position-addressed: the parser may rewind to any byte offset and re-lex in
//! a different mode (this is how statement dispatch re-interprets a head).

use shoal_ast::Span;

mod number;
mod string;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Cmd,
    Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    // --- layout ---
    Newline,
    Semi,
    Eof,

    // --- shared literals ---
    Int(i64),
    Float(f64),
    Size(u64),
    Duration(i64),
    Time {
        hour: u8,
        min: u8,
        sec: u8,
    },
    /// Fully-unescaped non-interpolating string.
    Str(String),
    /// Interpolating string: literal segments and embedded expression sources.
    StrInterp(Vec<Seg>),
    /// `re"…"` — raw regex source.
    Regex(String),
    /// `t"…"` — tagged datetime source.
    DateTime(String),

    // --- EXPR mode ---
    Ident(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Colon,
    Dot,
    DotDot,
    DotDotEq,
    QuestionDot,
    FatArrow,
    ThinArrow,
    Ellipsis,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    Bang,
    AndAnd,
    OrOr,
    QQ,
    Pipe,
    Caret,
    Question,

    // --- CMD mode ---
    /// Bare word (str).
    Word(String),
    /// Path literal word (`./x`, `~/x`, `/x`, `../x`) — raw text.
    PathWord(String),
    /// Glob word — raw pattern.
    GlobWord(String),
    /// `--name` (no inline value).
    FlagLong(String),
    /// `--name=value` (value is the raw word text after `=`).
    FlagLongEq(String, String),
    /// `--name=` with the value as the *next* token (quoted or parenthesized).
    FlagLongPendingValue(String),
    /// `-abc`
    FlagShort(String),
    DashDash,
    Dash,
    /// `IDENT=rest` env-prefix word: (name, raw value text; empty when the
    /// value is the next token).
    EnvAssign(String, String),
    RedirOut,
    RedirAppend,
    RedirIn,
    Amp,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Seg {
    Lit(String),
    /// Embedded `{expr}` — byte range of the expression source.
    Expr {
        start: u32,
        end: u32,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct LexError {
    pub msg: String,
    pub span: Span,
    pub hint: Option<String>,
}

impl LexError {
    fn new(msg: impl Into<String>, span: Span) -> LexError {
        LexError {
            msg: msg.into(),
            span,
            hint: None,
        }
    }
    fn hint(mut self, h: impl Into<String>) -> LexError {
        self.hint = Some(h.into());
        self
    }
}

pub type LexResult = Result<(Tok, Span), LexError>;

pub struct Lexer<'s> {
    pub src: &'s str,
    bytes: &'s [u8],
}

pub const RESERVED: &[&str] = &[
    "let", "var", "fn", "alias", "use", "export", "return", "break", "continue", "if", "else",
    "match", "for", "in", "while", "try", "catch", "true", "false", "null",
];

impl<'s> Lexer<'s> {
    pub fn new(src: &'s str) -> Lexer<'s> {
        Lexer {
            src,
            bytes: src.as_bytes(),
        }
    }

    fn at(&self, pos: usize) -> u8 {
        if pos < self.bytes.len() {
            self.bytes[pos]
        } else {
            0
        }
    }

    /// Skip spaces/tabs (not newlines), comments, and `\`-newline continuations.
    /// Returns the position of the next significant byte.
    pub fn skip_trivia(&self, mut pos: usize) -> usize {
        loop {
            match self.at(pos) {
                b' ' | b'\t' | b'\r' => pos += 1,
                b'\\' if self.at(pos + 1) == b'\n' => pos += 2,
                b'\\' if self.at(pos + 1) == b'\r' && self.at(pos + 2) == b'\n' => pos += 3,
                b'#' => {
                    // Comment only at token start; skip_trivia is only called
                    // at token boundaries, so `#` here always begins a comment.
                    while pos < self.bytes.len() && self.at(pos) != b'\n' {
                        pos += 1;
                    }
                }
                _ => return pos,
            }
        }
    }

    /// Lex one token at `pos` in `mode`. Returns token, span; the next
    /// position is the span's `end`.
    pub fn token(&self, pos: usize, mode: Mode) -> LexResult {
        if pos < self.src.len() && !self.src.is_char_boundary(pos) {
            return Err(LexError::new(
                "token offset is not a UTF-8 character boundary",
                Span::new(pos, pos),
            ));
        }
        let pos = self.skip_trivia(pos);
        if pos >= self.bytes.len() {
            return Ok((Tok::Eof, Span::new(pos, pos)));
        }
        let c = self.at(pos);
        match c {
            b'\n' => Ok((Tok::Newline, Span::new(pos, pos + 1))),
            b';' => Ok((Tok::Semi, Span::new(pos, pos + 1))),
            b'"' => self.string(pos),
            b'\'' => self.raw_string(pos),
            b'`' => Err(
                LexError::new("backtick is not shoal syntax", Span::new(pos, pos + 1))
                    .hint("command substitution is (cmd …); verbatim POSIX lives in sh { … }"),
            ),
            _ => match mode {
                Mode::Cmd => self.cmd_token(pos),
                Mode::Expr => self.expr_token(pos),
            },
        }
    }

    // -----------------------------------------------------------------
    // CMD mode
    // -----------------------------------------------------------------

    fn cmd_token(&self, pos: usize) -> LexResult {
        let c = self.at(pos);
        match c {
            b'(' => Ok((Tok::LParen, Span::new(pos, pos + 1))),
            b')' => Ok((Tok::RParen, Span::new(pos, pos + 1))),
            b'{' => Ok((Tok::LBrace, Span::new(pos, pos + 1))),
            b'}' => Ok((Tok::RBrace, Span::new(pos, pos + 1))),
            // A `[` beginning a word is a glob character class when a matching
            // `]` closes within the same whitespace-delimited run (`[abc].txt`,
            // site/content/internals/language-conformance-contract.md). A lone unclosed `[` keeps the teaching error.
            b'[' if self.bracket_class_closes(pos) => self.cmd_word(pos),
            b'[' => Err(LexError::new(
                "`[` cannot start a command argument",
                Span::new(pos, pos + 1),
            )
            .hint("quote it, or pass a list with (expr); mid-word [ranges] in globs are fine")),
            b']' => Err(LexError::new("unexpected `]`", Span::new(pos, pos + 1))),
            b'>' => {
                if self.at(pos + 1) == b'>' {
                    Ok((Tok::RedirAppend, Span::new(pos, pos + 2)))
                } else {
                    Ok((Tok::RedirOut, Span::new(pos, pos + 1)))
                }
            }
            b'<' => Ok((Tok::RedirIn, Span::new(pos, pos + 1))),
            b'&' => {
                if self.at(pos + 1) == b'&' {
                    Ok((Tok::AndAnd, Span::new(pos, pos + 2)))
                } else {
                    Ok((Tok::Amp, Span::new(pos, pos + 1)))
                }
            }
            b'|' => {
                if self.at(pos + 1) == b'|' {
                    Ok((Tok::OrOr, Span::new(pos, pos + 2)))
                } else {
                    Ok((Tok::Pipe, Span::new(pos, pos + 1)))
                }
            }
            b'^' => Ok((Tok::Caret, Span::new(pos, pos + 1))),
            _ => self.cmd_word(pos),
        }
    }

    /// Does a `[` at `pos` open a glob character class that closes with a `]`
    /// before the whitespace-delimited run ends (D10)?
    fn bracket_class_closes(&self, pos: usize) -> bool {
        let mut p = pos + 1;
        while p < self.bytes.len() {
            match self.at(p) {
                b' ' | b'\t' | b'\r' | b'\n' | b'(' | b')' | b'{' | b'}' | b'"' | b'\'' | b';'
                | b'&' | b'<' | b'>' | b'|' => return false,
                b']' => return true,
                _ => p += 1,
            }
        }
        false
    }

    /// Scan a CMD-mode word and classify it by shape (site/content/internals/language-conformance-contract.md).
    fn cmd_word(&self, start: usize) -> LexResult {
        let mut pos = start;
        let mut has_glob = false;
        while pos < self.bytes.len() {
            let c = self.at(pos);
            match c {
                b' ' | b'\t' | b'\r' | b'\n' | b'(' | b')' | b'{' | b'}' | b'"' | b'\'' | b';'
                | b'&' | b'<' | b'>' | b'|' => break,
                // A `]` breaks a word at word start; a leading `[` is only
                // reached here when `cmd_token` has already confirmed a matching
                // `]` closes the run, so it is a glob class (D10).
                b']' if pos == start => break,
                b'$' => {
                    return Err(LexError::new(
                        "shoal variables have no sigil",
                        Span::new(pos, pos + 1),
                    )
                    .hint("write `name`, not `$name`; environment variables are `env.NAME`"));
                }
                b'`' => {
                    return Err(LexError::new(
                        "backtick is not shoal syntax",
                        Span::new(pos, pos + 1),
                    )
                    .hint("command substitution is (cmd …); verbatim POSIX lives in sh { … }"));
                }
                b'*' | b'?' => {
                    has_glob = true;
                    pos += 1;
                }
                b'[' | b']' => {
                    has_glob = true;
                    pos += 1;
                }
                _ => pos += 1,
            }
        }
        let text = &self.src[start..pos];
        if text.is_empty() {
            return Err(LexError::new(
                "unexpected character",
                Span::new(start, start + 1),
            ));
        }
        let span = Span::new(start, pos);
        // Shape classification, in priority order.
        if text == "--" {
            return Ok((Tok::DashDash, span));
        }
        if text == "-" {
            return Ok((Tok::Dash, span));
        }
        if has_glob {
            return Ok((Tok::GlobWord(text.to_string()), span));
        }
        if text.starts_with("~/")
            || text == "~"
            || text.starts_with("./")
            || text.starts_with("../")
            || text.starts_with('/')
        {
            return Ok((Tok::PathWord(text.to_string()), span));
        }
        if let Some(rest) = text.strip_prefix("--") {
            // --ident or --ident=value
            let (name, val) = match rest.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None => (rest, None),
            };
            if !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                let name = name.replace('-', "_");
                return Ok(match val {
                    None => (Tok::FlagLong(name), span),
                    Some("") => (Tok::FlagLongPendingValue(name), span),
                    Some(v) => (Tok::FlagLongEq(name, v.to_string()), span),
                });
            }
            return Ok((Tok::Word(text.to_string()), span));
        }
        if let Some(rest) = text.strip_prefix('-') {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Ok((Tok::FlagShort(rest.to_string()), span));
            }
            return Ok((Tok::Word(text.to_string()), span));
        }
        // IDENT=rest (env-prefix shape; meaningful only at head position —
        // the parser decides).
        if let Some((name, rest)) = text.split_once('=') {
            if is_ident(name) {
                return Ok((Tok::EnvAssign(name.to_string(), rest.to_string()), span));
            }
        }
        Ok((Tok::Word(text.to_string()), span))
    }

    // -----------------------------------------------------------------
    // EXPR mode
    // -----------------------------------------------------------------

    fn expr_token(&self, pos: usize) -> LexResult {
        let c = self.at(pos);
        let one = |t| Ok((t, Span::new(pos, pos + 1)));
        let two = |t| Ok((t, Span::new(pos, pos + 2)));
        match c {
            b'(' => one(Tok::LParen),
            b')' => one(Tok::RParen),
            b'[' => one(Tok::LBracket),
            b']' => one(Tok::RBracket),
            b'{' => one(Tok::LBrace),
            b'}' => one(Tok::RBrace),
            b',' => one(Tok::Comma),
            b':' => one(Tok::Colon),
            b'+' => {
                if self.at(pos + 1) == b'=' {
                    two(Tok::PlusEq)
                } else {
                    one(Tok::Plus)
                }
            }
            b'-' => match self.at(pos + 1) {
                b'=' => two(Tok::MinusEq),
                b'>' => two(Tok::ThinArrow),
                _ => one(Tok::Minus),
            },
            b'*' => {
                if self.at(pos + 1) == b'=' {
                    two(Tok::StarEq)
                } else {
                    one(Tok::Star)
                }
            }
            b'/' => {
                if self.at(pos + 1) == b'=' {
                    two(Tok::SlashEq)
                } else {
                    one(Tok::Slash)
                }
            }
            b'%' => one(Tok::Percent),
            b'=' => match self.at(pos + 1) {
                b'=' => two(Tok::EqEq),
                b'>' => two(Tok::FatArrow),
                _ => one(Tok::Eq),
            },
            b'!' => {
                if self.at(pos + 1) == b'=' {
                    two(Tok::NotEq)
                } else {
                    one(Tok::Bang)
                }
            }
            b'<' => {
                if self.at(pos + 1) == b'=' {
                    two(Tok::Le)
                } else {
                    one(Tok::Lt)
                }
            }
            b'>' => {
                if self.at(pos + 1) == b'=' {
                    two(Tok::Ge)
                } else {
                    one(Tok::Gt)
                }
            }
            b'&' => {
                if self.at(pos + 1) == b'&' {
                    two(Tok::AndAnd)
                } else {
                    Err(
                        LexError::new("`&` is not an operator here", Span::new(pos, pos + 1))
                            .hint("backgrounding with & is command syntax; logical and is &&"),
                    )
                }
            }
            b'|' => {
                if self.at(pos + 1) == b'|' {
                    two(Tok::OrOr)
                } else {
                    one(Tok::Pipe)
                }
            }
            b'?' => match self.at(pos + 1) {
                b'?' => two(Tok::QQ),
                b'.' => two(Tok::QuestionDot),
                _ => one(Tok::Question),
            },
            b'.' => {
                if self.at(pos + 1) == b'.' {
                    if self.at(pos + 2) == b'=' {
                        Ok((Tok::DotDotEq, Span::new(pos, pos + 3)))
                    } else if self.at(pos + 2) == b'.' {
                        Ok((Tok::Ellipsis, Span::new(pos, pos + 3)))
                    } else {
                        two(Tok::DotDot)
                    }
                } else {
                    one(Tok::Dot)
                }
            }
            b'^' => one(Tok::Caret),
            b'$' => Err(
                LexError::new("shoal variables have no sigil", Span::new(pos, pos + 1))
                    .hint("write `name`, not `$name`; environment variables are `env.NAME`"),
            ),
            b'0'..=b'9' => self.number(pos),
            c if c == b'_' || c.is_ascii_alphabetic() => self.ident_or_tagged(pos),
            _ => {
                // Non-ASCII identifier start? Keep identifiers ASCII per site/content/internals/language-conformance-contract.md.
                Err(LexError::new(
                    format!(
                        "unexpected character `{}`",
                        self.src[pos..].chars().next().unwrap()
                    ),
                    Span::new(pos, pos + 1),
                ))
            }
        }
    }

    fn ident_or_tagged(&self, start: usize) -> LexResult {
        let mut pos = start;
        while pos < self.bytes.len()
            && (self.at(pos).is_ascii_alphanumeric() || self.at(pos) == b'_')
        {
            pos += self.src[pos..]
                .chars()
                .next()
                .expect("position is in bounds")
                .len_utf8();
        }
        let text = &self.src[start..pos];
        // Tagged literals: re"…", t"…"
        if self.at(pos) == b'"' {
            match text {
                "re" => return self.tagged_raw(start, pos),
                "t" => {
                    let (tok, span) = self.tagged_raw(start, pos)?;
                    if let Tok::Regex(src) = tok {
                        return Ok((Tok::DateTime(src), span));
                    }
                    unreachable!()
                }
                _ => {}
            }
        }
        Ok((Tok::Ident(text.to_string()), Span::new(start, pos)))
    }

    /// Scan `tag"…"` with raw semantics (no escapes, no interpolation).
    fn tagged_raw(&self, start: usize, quote: usize) -> LexResult {
        let mut pos = quote + 1;
        while pos < self.bytes.len() && self.at(pos) != b'"' {
            pos += 1;
        }
        if pos >= self.bytes.len() {
            return Err(LexError::new(
                "unterminated tagged literal",
                Span::new(start, pos),
            ));
        }
        Ok((
            Tok::Regex(self.src[quote + 1..pos].to_string()),
            Span::new(start, pos + 1),
        ))
    }

    /// Scan a raw `{ … }` block (for `sh { … }`), balanced braces outside
    /// quotes (site/content/internals/language-conformance-contract.md). `open` is the position of `{`. Returns (payload,
    /// end position after `}`).
    pub fn raw_brace_block(&self, open: usize) -> Result<(String, usize), LexError> {
        if open >= self.src.len() || !self.src.is_char_boundary(open) || self.at(open) != b'{' {
            let pos = open.min(self.src.len());
            return Err(LexError::new(
                "raw block offset does not point to `{`",
                Span::new(pos, pos),
            ));
        }
        let mut pos = open + 1;
        let mut depth = 1usize;
        while pos < self.bytes.len() {
            match self.at(pos) {
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        let payload = self.src[open + 1..pos].trim().to_string();
                        return Ok((payload, pos + 1));
                    }
                }
                b'"' => {
                    pos += 1;
                    while pos < self.bytes.len() && self.at(pos) != b'"' {
                        if self.at(pos) == b'\\' {
                            pos += 1;
                        }
                        pos += 1;
                    }
                }
                b'\'' => {
                    pos += 1;
                    while pos < self.bytes.len() && self.at(pos) != b'\'' {
                        pos += 1;
                    }
                }
                _ => {}
            }
            pos += 1;
        }
        Err(
            // site/content/internals/values-streams-execution.md `lang_block_unbalanced`: brace scan hit EOF with a
            // still-open `{`. Message stays tool-agnostic (the scanner is shared
            // across every interpreter block, `sh`/`python`/`jq`/…).
            LexError::new(
                "unbalanced braces in interpreter block `{ … }`",
                Span::new(open, pos),
            )
            .hint("for payloads with unbalanced braces use the triple-raw form: tool ''' … '''"),
        )
    }
}

pub fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok(src: &str, mode: Mode) -> Tok {
        Lexer::new(src).token(0, mode).unwrap().0
    }

    #[test]
    fn numbers_and_units() {
        assert_eq!(tok("123", Mode::Expr), Tok::Int(123));
        assert_eq!(tok("1_000_000", Mode::Expr), Tok::Int(1_000_000));
        assert_eq!(tok("0xFF", Mode::Expr), Tok::Int(255));
        assert_eq!(tok("0o755", Mode::Expr), Tok::Int(493));
        assert_eq!(tok("0b1010", Mode::Expr), Tok::Int(10));
        assert_eq!(tok("3.14", Mode::Expr), Tok::Float(3.14));
        assert_eq!(tok("1e9", Mode::Expr), Tok::Float(1e9));
        assert_eq!(tok("1.5gb", Mode::Expr), Tok::Size(1_500_000_000));
        assert_eq!(tok("4kib", Mode::Expr), Tok::Size(4096));
        assert_eq!(tok("250ms", Mode::Expr), Tok::Duration(250_000_000));
        assert_eq!(
            tok("10:00am", Mode::Expr),
            Tok::Time {
                hour: 10,
                min: 0,
                sec: 0
            }
        );
        assert_eq!(
            tok("23:15", Mode::Expr),
            Tok::Time {
                hour: 23,
                min: 15,
                sec: 0
            }
        );
    }

    #[test]
    fn int_dot_is_not_float() {
        let lx = Lexer::new("1..5");
        let (t1, s1) = lx.token(0, Mode::Expr).unwrap();
        assert_eq!(t1, Tok::Int(1));
        let (t2, _) = lx.token(s1.end as usize, Mode::Expr).unwrap();
        assert_eq!(t2, Tok::DotDot);
    }

    #[test]
    fn strings() {
        assert_eq!(tok(r#""hello""#, Mode::Expr), Tok::Str("hello".into()));
        assert_eq!(tok(r#""a\nb""#, Mode::Expr), Tok::Str("a\nb".into()));
        assert_eq!(tok("'raw \\n'", Mode::Expr), Tok::Str("raw \\n".into()));
        match tok(r#""x {1 + 2} y""#, Mode::Expr) {
            Tok::StrInterp(segs) => {
                assert_eq!(segs.len(), 3);
                assert_eq!(segs[0], Seg::Lit("x ".into()));
                assert!(matches!(segs[1], Seg::Expr { .. }));
            }
            other => panic!("expected interp, got {other:?}"),
        }
        assert_eq!(tok(r#"re"\d+""#, Mode::Expr), Tok::Regex("\\d+".into()));
        assert_eq!(
            tok(r#"t"2026-07-09""#, Mode::Expr),
            Tok::DateTime("2026-07-09".into())
        );
    }

    #[test]
    fn cmd_words() {
        assert_eq!(tok("push", Mode::Cmd), Tok::Word("push".into()));
        assert_eq!(
            tok("./deploy.sh", Mode::Cmd),
            Tok::PathWord("./deploy.sh".into())
        );
        assert_eq!(tok("~/x", Mode::Cmd), Tok::PathWord("~/x".into()));
        assert_eq!(tok("*.rs", Mode::Cmd), Tok::GlobWord("*.rs".into()));
        assert_eq!(
            tok("src/**/*.rs", Mode::Cmd),
            Tok::GlobWord("src/**/*.rs".into())
        );
        assert_eq!(
            tok("file[0-9].txt", Mode::Cmd),
            Tok::GlobWord("file[0-9].txt".into())
        );
        assert_eq!(tok("--release", Mode::Cmd), Tok::FlagLong("release".into()));
        assert_eq!(
            tok("--jobs=4", Mode::Cmd),
            Tok::FlagLongEq("jobs".into(), "4".into())
        );
        assert_eq!(tok("--dry-run", Mode::Cmd), Tok::FlagLong("dry_run".into()));
        assert_eq!(tok("-rf", Mode::Cmd), Tok::FlagShort("rf".into()));
        assert_eq!(tok("--", Mode::Cmd), Tok::DashDash);
        assert_eq!(tok("-", Mode::Cmd), Tok::Dash);
        assert_eq!(
            tok("NAME=v", Mode::Cmd),
            Tok::EnvAssign("NAME".into(), "v".into())
        );
        assert_eq!(tok("ver#2", Mode::Cmd), Tok::Word("ver#2".into()));
    }

    #[test]
    fn sigil_errors() {
        assert!(Lexer::new("$HOME").token(0, Mode::Cmd).is_err());
        assert!(Lexer::new("$x").token(0, Mode::Expr).is_err());
        assert!(Lexer::new("`ls`").token(0, Mode::Expr).is_err());
    }

    #[test]
    fn comments_only_at_token_start() {
        let lx = Lexer::new("  # comment\nx");
        let (t, _) = lx.token(0, Mode::Expr).unwrap();
        assert_eq!(t, Tok::Newline);
    }

    #[test]
    fn raw_block() {
        let lx = Lexer::new("{ echo \"}\" | wc }");
        let (payload, end) = lx.raw_brace_block(0).unwrap();
        assert_eq!(payload, "echo \"}\" | wc");
        assert_eq!(end, 17);
    }

    #[test]
    fn triple_dedent() {
        let src = "\"\"\"\n    line1\n      line2\n    \"\"\"";
        match tok(src, Mode::Expr) {
            Tok::Str(s) => assert_eq!(s, "line1\n  line2"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unterminated_raw_unicode_never_slices_inside_codepoint() {
        let result = Lexer::new("'𐣠").token(0, Mode::Expr);
        assert!(matches!(result, Err(LexError { ref msg, .. }) if msg.contains("unterminated")));
    }

    #[test]
    fn caller_supplied_offsets_never_panic_on_utf8_or_overflow() {
        let lexer = Lexer::new("é{}");
        let error = lexer.token(1, Mode::Cmd).unwrap_err();
        assert!(error.msg.contains("UTF-8"));
        assert!(lexer.raw_brace_block(usize::MAX).is_err());
        assert!(lexer.raw_brace_block(1).is_err());
    }

    /// A literal open-brace in an interpolating string starts a `{expr}`
    /// interpolation (by design — site/content/internals/language-conformance-contract.md). When it can't find a matching
    /// close (a realistic dogfooding case: JSON text with escaped `"` whose
    /// nested-string scan runs off the end looking for the interpolation's
    /// `}`), the error must teach the fix rather than just say "unterminated".
    #[test]
    fn unterminated_interpolation_hint_teaches_the_fix() {
        let err = Lexer::new(r#""{\"key\": \"value\"}""#)
            .token(0, Mode::Expr)
            .unwrap_err();
        assert!(err.msg.contains("`{expr}`"), "{err:?}");
        let hint = err.hint.expect("should carry a hint");
        assert!(hint.contains("raw string"), "{hint}");
        assert!(hint.contains('\''), "{hint}");
        assert!(hint.contains('\\'), "{hint}");
    }
}
