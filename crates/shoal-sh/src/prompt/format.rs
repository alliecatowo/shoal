//! The format-string mini-language (site/content/internals/prompt-editor-lsp.md).
//!
//! Each `format.*` string is parsed once at config-load time into a
//! `Vec<FormatToken>` and cached for the process lifetime — reparsing on every
//! render would itself violate the site/content/internals/prompt-editor-lsp.md budget. Syntax is deliberately
//! Starship-compatible: `$module`, `[text](style)`, literal passthrough.

/// One node of a parsed format string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatToken {
    /// Literal text. `ws_only` marks a run that is entirely whitespace, so the
    /// whitespace-collapse rule (site/content/internals/prompt-editor-lsp.md) can drop it next to an empty module.
    Literal { text: String, ws_only: bool },
    /// `$ident` — a module placeholder.
    Placeholder(String),
    /// `[ ... ](style)` — a style group; the style applies to everything inside.
    Group {
        inner: Vec<FormatToken>,
        style: String,
    },
}

/// Parse a format string into tokens. This is intentionally lenient: a stray
/// `[` with no matching `](...)` is treated as literal text rather than a parse
/// error, matching the project's warn-don't-crash posture for config.
pub fn parse_format(input: &str) -> Vec<FormatToken> {
    let chars: Vec<char> = input.chars().collect();
    let mut pos = 0;
    parse_seq(&chars, &mut pos, false)
}

/// Parse a token sequence. When `in_group`, a top-level `]` ends the sequence
/// (returning to the caller, which then consumes the `(style)` suffix).
fn parse_seq(chars: &[char], pos: &mut usize, in_group: bool) -> Vec<FormatToken> {
    let mut out: Vec<FormatToken> = Vec::new();
    let mut lit = String::new();

    let flush = |lit: &mut String, out: &mut Vec<FormatToken>| {
        if !lit.is_empty() {
            let ws_only = lit.chars().all(char::is_whitespace);
            out.push(FormatToken::Literal {
                text: std::mem::take(lit),
                ws_only,
            });
        }
    };

    while *pos < chars.len() {
        let c = chars[*pos];
        match c {
            ']' if in_group => {
                // Caller handles the closing bracket + style suffix.
                break;
            }
            '$' => {
                let ident = read_ident(chars, *pos + 1);
                if ident.is_empty() {
                    lit.push('$');
                    *pos += 1;
                } else {
                    flush(&mut lit, &mut out);
                    *pos += 1 + ident.chars().count();
                    out.push(FormatToken::Placeholder(ident));
                }
            }
            '[' => {
                // Try to parse a style group; on failure treat '[' as literal.
                if let Some((token, next)) = try_group(chars, *pos) {
                    flush(&mut lit, &mut out);
                    out.push(token);
                    *pos = next;
                } else {
                    lit.push('[');
                    *pos += 1;
                }
            }
            _ => {
                lit.push(c);
                *pos += 1;
            }
        }
    }
    flush(&mut lit, &mut out);
    out
}

/// Attempt to parse `[ inner ]( style )` starting at `open` (a `[`). Returns the
/// group token and the position just past the closing `)` on success.
fn try_group(chars: &[char], open: usize) -> Option<(FormatToken, usize)> {
    let mut pos = open + 1;
    let inner = parse_seq(chars, &mut pos, true);
    // Must be sitting on the matching ']'.
    if chars.get(pos) != Some(&']') {
        return None;
    }
    pos += 1;
    // Immediately followed by '('.
    if chars.get(pos) != Some(&'(') {
        return None;
    }
    pos += 1;
    let mut style = String::new();
    while pos < chars.len() && chars[pos] != ')' {
        style.push(chars[pos]);
        pos += 1;
    }
    if chars.get(pos) != Some(&')') {
        return None;
    }
    pos += 1;
    Some((
        FormatToken::Group {
            inner,
            style: style.trim().to_string(),
        },
        pos,
    ))
}

fn read_ident(chars: &[char], start: usize) -> String {
    let mut s = String::new();
    let mut i = start;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_alphanumeric() || c == '_' {
            s.push(c);
            i += 1;
        } else {
            break;
        }
    }
    s
}

/// Every `$placeholder` id referenced anywhere in `tokens` (recursing into
/// groups) — used at load time to warn about unknown module ids (site/content/internals/prompt-editor-lsp.md).
pub fn referenced_ids(tokens: &[FormatToken]) -> Vec<String> {
    let mut ids = Vec::new();
    collect_ids(tokens, &mut ids);
    ids
}

fn collect_ids(tokens: &[FormatToken], out: &mut Vec<String>) {
    for t in tokens {
        match t {
            FormatToken::Placeholder(id) => out.push(id.clone()),
            FormatToken::Group { inner, .. } => collect_ids(inner, out),
            FormatToken::Literal { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_placeholder() {
        assert_eq!(
            parse_format("$directory"),
            vec![FormatToken::Placeholder("directory".into())]
        );
    }

    #[test]
    fn adjacent_placeholders_and_literal() {
        assert_eq!(
            parse_format("$directory $git_branch"),
            vec![
                FormatToken::Placeholder("directory".into()),
                FormatToken::Literal {
                    text: " ".into(),
                    ws_only: true
                },
                FormatToken::Placeholder("git_branch".into()),
            ]
        );
    }

    #[test]
    fn style_group_and_nested() {
        let toks = parse_format("[$directory](cyan bold)");
        assert_eq!(
            toks,
            vec![FormatToken::Group {
                inner: vec![FormatToken::Placeholder("directory".into())],
                style: "cyan bold".into()
            }]
        );
        let nested = parse_format("[a [b](red)](green)");
        match &nested[0] {
            FormatToken::Group { inner, style } => {
                assert_eq!(style, "green");
                assert!(matches!(inner[1], FormatToken::Group { .. }));
            }
            _ => panic!("expected group"),
        }
    }

    #[test]
    fn unmatched_bracket_is_literal() {
        assert_eq!(
            parse_format("[oops"),
            vec![FormatToken::Literal {
                text: "[oops".into(),
                ws_only: false
            }]
        );
    }

    #[test]
    fn lone_dollar_is_literal() {
        assert_eq!(
            parse_format("$ x"),
            vec![FormatToken::Literal {
                text: "$ x".into(),
                ws_only: false
            }]
        );
    }

    #[test]
    fn newline_literal() {
        let toks = parse_format("$a\n$b");
        assert_eq!(
            toks[1],
            FormatToken::Literal {
                text: "\n".into(),
                ws_only: true
            }
        );
    }

    #[test]
    fn referenced_ids_recurses_groups() {
        let ids = referenced_ids(&parse_format("$directory[$git_branch](red)"));
        assert_eq!(ids, vec!["directory".to_string(), "git_branch".to_string()]);
    }
}
