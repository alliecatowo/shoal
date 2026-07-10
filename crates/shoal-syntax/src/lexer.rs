//! The modal lexer (TDD §2). The parser drives the lexer between `CMD` mode
//! (command word soup) and `EXPR` mode (conventional tokens); mode is a
//! property of grammar position, never runtime state. The lexer is
//! position-addressed: the parser may rewind to any byte offset and re-lex in
//! a different mode (this is how statement dispatch re-interprets a head).

use shoal_ast::Span;

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

    /// Scan a CMD-mode word and classify it by shape (TDD §2.2).
    fn cmd_word(&self, start: usize) -> LexResult {
        let mut pos = start;
        let mut has_glob = false;
        while pos < self.bytes.len() {
            let c = self.at(pos);
            match c {
                b' ' | b'\t' | b'\r' | b'\n' | b'(' | b')' | b'{' | b'}' | b'"' | b'\'' | b';'
                | b'&' | b'<' | b'>' | b'|' => break,
                // `[`/`]` break words at word start only; mid-word they are
                // glob character classes.
                b'[' | b']' if pos == start => break,
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
                // Non-ASCII identifier start? Keep identifiers ASCII per TDD §2.3.
                Err(LexError::new(
                    format!(
                        "unexpected character `{}`",
                        &self.src[pos..].chars().next().unwrap()
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

    fn number(&self, start: usize) -> LexResult {
        let mut pos = start;
        // Radix prefixes.
        if self.at(pos) == b'0'
            && matches!(self.at(pos + 1), b'x' | b'X' | b'o' | b'O' | b'b' | b'B')
        {
            let radix = match self.at(pos + 1) {
                b'x' | b'X' => 16,
                b'o' | b'O' => 8,
                _ => 2,
            };
            pos += 2;
            let digits_start = pos;
            while pos < self.bytes.len()
                && (self.at(pos).is_ascii_alphanumeric() || self.at(pos) == b'_')
            {
                pos += 1;
            }
            let digits: String = self.src[digits_start..pos]
                .chars()
                .filter(|&c| c != '_')
                .collect();
            let v = i64::from_str_radix(&digits, radix)
                .map_err(|_| LexError::new("invalid numeric literal", Span::new(start, pos)))?;
            return Ok((Tok::Int(v), Span::new(start, pos)));
        }

        let mut is_float = false;
        while pos < self.bytes.len() && (self.at(pos).is_ascii_digit() || self.at(pos) == b'_') {
            pos += 1;
        }
        // Time literal: \d{1,2}:\d{2}(:\d{2})?(am|pm)?
        if self.at(pos) == b':' && self.at(pos + 1).is_ascii_digit() {
            if let Some(res) = self.try_time(start, pos) {
                return res;
            }
        }
        // Fraction — but not `..` (range) and not `.method`.
        if self.at(pos) == b'.' && self.at(pos + 1).is_ascii_digit() {
            is_float = true;
            pos += 1;
            while pos < self.bytes.len() && (self.at(pos).is_ascii_digit() || self.at(pos) == b'_')
            {
                pos += 1;
            }
        }
        // Exponent.
        if matches!(self.at(pos), b'e' | b'E') {
            let mut p = pos + 1;
            if matches!(self.at(p), b'+' | b'-') {
                p += 1;
            }
            if self.at(p).is_ascii_digit() {
                is_float = true;
                pos = p;
                while pos < self.bytes.len() && self.at(pos).is_ascii_digit() {
                    pos += 1;
                }
            }
        }
        let num_text: String = self.src[start..pos].chars().filter(|&c| c != '_').collect();

        // Maximal munch: unit suffix folds into a single size/duration literal.
        let unit_start = pos;
        let mut upos = pos;
        while upos < self.bytes.len() && self.at(upos).is_ascii_alphabetic() {
            upos += 1;
        }
        let unit = &self.src[unit_start..upos];
        if !unit.is_empty() {
            let lower = unit.to_ascii_lowercase();
            const SIZE_UNITS: &[&str] = &["b", "kb", "mb", "gb", "tb", "kib", "mib", "gib", "tib"];
            const DUR_UNITS: &[&str] = &["ns", "us", "ms", "s", "m", "h", "d", "w"];
            if SIZE_UNITS.contains(&lower.as_str()) {
                let word = format!("{num_text}{lower}");
                let v = shoal_value_parse_size(&word)
                    .ok_or_else(|| LexError::new("invalid size literal", Span::new(start, upos)))?;
                return Ok((Tok::Size(v), Span::new(start, upos)));
            }
            if DUR_UNITS.contains(&lower.as_str()) {
                let word = format!("{num_text}{lower}");
                let v = shoal_value_parse_duration(&word).ok_or_else(|| {
                    LexError::new("invalid duration literal", Span::new(start, upos))
                })?;
                return Ok((Tok::Duration(v), Span::new(start, upos)));
            }
            return Err(LexError::new(
                format!("unknown unit `{unit}` on numeric literal"),
                Span::new(start, upos),
            )
            .hint("sizes: b kb mb gb tb kib mib gib tib; durations: ns us ms s m h d w"));
        }

        if is_float {
            let v: f64 = num_text
                .parse()
                .map_err(|_| LexError::new("invalid float literal", Span::new(start, pos)))?;
            Ok((Tok::Float(v), Span::new(start, pos)))
        } else {
            let v: i64 = num_text.parse().map_err(|_| {
                LexError::new("integer literal out of range", Span::new(start, pos))
            })?;
            Ok((Tok::Int(v), Span::new(start, pos)))
        }
    }

    fn try_time(&self, start: usize, colon: usize) -> Option<LexResult> {
        let hour_txt = &self.src[start..colon];
        if hour_txt.len() > 2 || hour_txt.contains('_') {
            return None;
        }
        let hour: u8 = hour_txt.parse().ok()?;
        let mut pos = colon + 1;
        let min_start = pos;
        while pos < self.bytes.len() && self.at(pos).is_ascii_digit() {
            pos += 1;
        }
        if pos - min_start != 2 {
            return None;
        }
        let min: u8 = self.src[min_start..pos].parse().ok()?;
        let mut sec: u8 = 0;
        if self.at(pos) == b':' && self.at(pos + 1).is_ascii_digit() {
            let sec_start = pos + 1;
            let mut p = sec_start;
            while p < self.bytes.len() && self.at(p).is_ascii_digit() {
                p += 1;
            }
            if p - sec_start == 2 {
                sec = self.src[sec_start..p].parse().ok()?;
                pos = p;
            } else {
                return None;
            }
        }
        let mut hour = hour;
        // am/pm suffix
        let rest = &self.src[pos..];
        if rest.len() >= 2 {
            let suf = &rest[..2].to_ascii_lowercase();
            if suf == "am" || suf == "pm" {
                if hour == 0 || hour > 12 {
                    return None;
                }
                if suf == "pm" && hour != 12 {
                    hour += 12;
                }
                if suf == "am" && hour == 12 {
                    hour = 0;
                }
                pos += 2;
            }
        }
        if hour > 23 || min > 59 || sec > 59 {
            return Some(Err(LexError::new(
                "invalid time literal",
                Span::new(start, pos),
            )));
        }
        Some(Ok((Tok::Time { hour, min, sec }, Span::new(start, pos))))
    }

    // -----------------------------------------------------------------
    // Strings
    // -----------------------------------------------------------------

    /// `"…"` interpolating; `"""…"""` multiline with dedent.
    fn string(&self, start: usize) -> LexResult {
        let triple = self.src[start..].starts_with("\"\"\"");
        let (open_len, close): (usize, &str) = if triple { (3, "\"\"\"") } else { (1, "\"") };
        let body_start = start + open_len;
        let mut pos = body_start;
        let mut segs: Vec<Seg> = Vec::new();
        let mut lit = String::new();

        loop {
            if pos >= self.bytes.len() {
                return Err(LexError::new("unterminated string", Span::new(start, pos)));
            }
            if self.src[pos..].starts_with(close) {
                if !triple && self.at(pos) == b'"' && close == "\"" {
                    // fallthrough: single close below
                }
                break;
            }
            let c = self.at(pos);
            match c {
                b'\\' => {
                    let (ch, len) = self.escape(pos)?;
                    lit.push_str(&ch);
                    pos += len;
                }
                b'{' => {
                    // Embedded expression.
                    let expr_start = pos + 1;
                    let expr_end = self.find_interp_end(expr_start).ok_or_else(|| {
                        LexError::new("unterminated `{expr}` in string", Span::new(pos, pos + 1))
                            .hint("escape a literal brace as \\{")
                    })?;
                    if !lit.is_empty() {
                        segs.push(Seg::Lit(std::mem::take(&mut lit)));
                    }
                    segs.push(Seg::Expr {
                        start: expr_start as u32,
                        end: expr_end as u32,
                    });
                    pos = expr_end + 1;
                }
                b'\n' if !triple => {
                    return Err(LexError::new(
                        "unterminated string (newline)",
                        Span::new(start, pos),
                    )
                    .hint("use \"\"\"…\"\"\" for multiline strings"));
                }
                _ => {
                    let ch = self.src[pos..].chars().next().unwrap();
                    lit.push(ch);
                    pos += ch.len_utf8();
                }
            }
        }
        let end = pos + open_len;
        if !lit.is_empty() {
            segs.push(Seg::Lit(lit));
        }
        // Dedent triple strings.
        if triple {
            segs = dedent_segs(segs);
        }
        let span = Span::new(start, end);
        if segs.iter().all(|s| matches!(s, Seg::Lit(_))) {
            let joined: String = segs
                .into_iter()
                .map(|s| match s {
                    Seg::Lit(t) => t,
                    _ => unreachable!(),
                })
                .collect();
            Ok((Tok::Str(joined), span))
        } else {
            Ok((Tok::StrInterp(segs), span))
        }
    }

    /// Find the `}` closing an interpolation, respecting nested braces and
    /// inner string literals.
    fn find_interp_end(&self, start: usize) -> Option<usize> {
        let mut pos = start;
        let mut depth = 0usize;
        while pos < self.bytes.len() {
            match self.at(pos) {
                b'{' => {
                    depth += 1;
                    pos += 1;
                }
                b'}' => {
                    if depth == 0 {
                        return Some(pos);
                    }
                    depth -= 1;
                    pos += 1;
                }
                b'"' => {
                    pos += 1;
                    while pos < self.bytes.len() && self.at(pos) != b'"' {
                        if self.at(pos) == b'\\' {
                            pos += 1;
                        }
                        pos += 1;
                    }
                    pos += 1;
                }
                b'\'' => {
                    pos += 1;
                    while pos < self.bytes.len() && self.at(pos) != b'\'' {
                        pos += 1;
                    }
                    pos += 1;
                }
                _ => pos += 1,
            }
        }
        None
    }

    fn escape(&self, pos: usize) -> Result<(String, usize), LexError> {
        match self.at(pos + 1) {
            b'n' => Ok(("\n".into(), 2)),
            b't' => Ok(("\t".into(), 2)),
            b'r' => Ok(("\r".into(), 2)),
            b'0' => Ok(("\0".into(), 2)),
            b'\\' => Ok(("\\".into(), 2)),
            b'"' => Ok(("\"".into(), 2)),
            b'{' => Ok(("{".into(), 2)),
            b'}' => Ok(("}".into(), 2)),
            b'u' if self.at(pos + 2) == b'{' => {
                let close = self.src[pos + 3..]
                    .find('}')
                    .ok_or_else(|| LexError::new("unterminated \\u{…}", Span::new(pos, pos + 3)))?;
                let hex = &self.src[pos + 3..pos + 3 + close];
                let cp = u32::from_str_radix(hex, 16).map_err(|_| {
                    LexError::new("invalid \\u{…} escape", Span::new(pos, pos + 3 + close))
                })?;
                let ch = char::from_u32(cp).ok_or_else(|| {
                    LexError::new("invalid unicode codepoint", Span::new(pos, pos + 3 + close))
                })?;
                Ok((ch.to_string(), 3 + close + 1))
            }
            other => Err(LexError::new(
                format!("unknown escape `\\{}`", other as char),
                Span::new(pos, pos + 2),
            )
            .hint("escapes: \\n \\t \\r \\0 \\\\ \\\" \\{ \\} \\u{…}")),
        }
    }

    /// `'…'` raw (zero escapes) and `'''…'''` multiline raw.
    fn raw_string(&self, start: usize) -> LexResult {
        let triple = self.src[start..].starts_with("'''");
        let open_len = if triple { 3 } else { 1 };
        let close = if triple { "'''" } else { "'" };
        let mut pos = start + open_len;
        while pos < self.bytes.len() && !self.src[pos..].starts_with(close) {
            if !triple && self.at(pos) == b'\n' {
                return Err(
                    LexError::new("unterminated string (newline)", Span::new(start, pos))
                        .hint("use '''…''' for multiline raw strings"),
                );
            }
            pos += self.src[pos..]
                .chars()
                .next()
                .expect("position is in bounds")
                .len_utf8();
        }
        if pos >= self.bytes.len() {
            return Err(LexError::new(
                "unterminated raw string",
                Span::new(start, pos),
            ));
        }
        let mut text = self.src[start + open_len..pos].to_string();
        if triple {
            text = dedent(&text);
        }
        Ok((Tok::Str(text), Span::new(start, pos + open_len)))
    }

    /// Scan a raw `{ … }` block (for `sh { … }`), balanced braces outside
    /// quotes (TDD §13.13). `open` is the position of `{`. Returns (payload,
    /// end position after `}`).
    pub fn raw_brace_block(&self, open: usize) -> Result<(String, usize), LexError> {
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
            LexError::new("unterminated sh { … } block", Span::new(open, pos))
                .hint("for payloads with unbalanced braces use sh''' … '''"),
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

/// Java-text-block style dedent: strip the common leading whitespace of
/// non-blank lines; drop a leading newline and trailing newline+indent.
fn dedent(text: &str) -> String {
    let text = text.strip_prefix('\n').unwrap_or(text);
    let text = match text.rfind('\n') {
        Some(i) if text[i + 1..].chars().all(|c| c == ' ' || c == '\t') => &text[..i],
        _ => text,
    };
    let indent = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    text.lines()
        .map(|l| {
            if l.len() >= indent {
                &l[indent..]
            } else {
                l.trim_start()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn dedent_segs(segs: Vec<Seg>) -> Vec<Seg> {
    // Dedent applies to literal text only; expression segments are opaque.
    // Compute indent across the literal content as if joined.
    let joined: String = segs
        .iter()
        .map(|s| match s {
            Seg::Lit(t) => t.clone(),
            Seg::Expr { .. } => "\u{FFFC}".to_string(), // placeholder, never a line start issue
        })
        .collect();
    let indent = {
        let t = joined.strip_prefix('\n').unwrap_or(&joined);
        t.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.len() - l.trim_start().len())
            .min()
            .unwrap_or(0)
    };
    let mut out = Vec::with_capacity(segs.len());
    let mut at_line_start = true;
    let mut first = true;
    for seg in segs {
        match seg {
            Seg::Lit(t) => {
                let mut s = String::with_capacity(t.len());
                let mut text = t.as_str();
                if first {
                    text = text.strip_prefix('\n').unwrap_or(text);
                    first = false;
                }
                let mut pending = 0usize;
                for ch in text.chars() {
                    if at_line_start && pending < indent && (ch == ' ' || ch == '\t') {
                        pending += 1;
                        continue;
                    }
                    at_line_start = false;
                    if ch == '\n' {
                        at_line_start = true;
                        pending = 0;
                    }
                    s.push(ch);
                }
                // strip trailing newline+indent
                if let Some(i) = s.rfind('\n') {
                    if s[i + 1..].chars().all(|c| c == ' ' || c == '\t') {
                        s.truncate(i);
                    }
                }
                out.push(Seg::Lit(s));
            }
            e => {
                first = false;
                at_line_start = false;
                out.push(e);
            }
        }
    }
    out
}

// Local copies of the unit parsers (shoal-syntax depends only on shoal-ast;
// keep the tiny parsing logic in sync with shoal-value::parse_size/duration).
fn shoal_value_parse_size(word: &str) -> Option<u64> {
    let split = word.find(|c: char| c.is_ascii_alphabetic())?;
    let (num, unit) = word.split_at(split);
    let num: f64 = num.parse().ok()?;
    let mult: f64 = match unit {
        "b" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        "kib" => 1024.0,
        "mib" => 1_048_576.0,
        "gib" => 1_073_741_824.0,
        "tib" => 1_099_511_627_776.0,
        _ => return None,
    };
    if num < 0.0 {
        return None;
    }
    Some((num * mult).round() as u64)
}

fn shoal_value_parse_duration(word: &str) -> Option<i64> {
    let split = word.find(|c: char| c.is_ascii_alphabetic())?;
    let (num, unit) = word.split_at(split);
    let num: f64 = num.parse().ok()?;
    let ns: f64 = match unit {
        "ns" => 1.0,
        "us" => 1e3,
        "ms" => 1e6,
        "s" => 1e9,
        "m" => 60e9,
        "h" => 3_600e9,
        "d" => 86_400e9,
        "w" => 604_800e9,
        _ => return None,
    };
    Some((num * ns).round() as i64)
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
}
