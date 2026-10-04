//! String scanning: interpolating `"…"` (with embedded `{expr}` segments and
//! escapes), raw `'…'`, their triple-quoted multiline forms (with dedent),
//! and the shared dedent helpers.

use super::*;

impl<'s> Lexer<'s> {
    /// `"…"` interpolating; `"""…"""` multiline with dedent.
    pub(crate) fn string(&self, start: usize) -> LexResult {
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
                            .hint(
                                "this `{` starts an interpolation — for literal braces, use a \
                                 raw string ('…') instead, or escape each brace as \\{ and \\}",
                            )
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
    pub(crate) fn raw_string(&self, start: usize) -> LexResult {
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
