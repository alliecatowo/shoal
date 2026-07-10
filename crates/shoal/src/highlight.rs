use nu_ansi_term::{Color, Style};
use reedline::{Highlighter, StyledText};
use shoal_syntax::{Lexer, Mode, Tok};

pub struct ShoalHighlighter;

fn is_keyword(name: &str) -> bool {
    matches!(
        name,
        "let"
            | "var"
            | "fn"
            | "if"
            | "else"
            | "match"
            | "for"
            | "in"
            | "while"
            | "return"
            | "break"
            | "continue"
            | "try"
            | "catch"
            | "alias"
            | "use"
            | "export"
    )
}

fn is_valid_command(cmd: &str) -> bool {
    let builtins = [
        "cd", "pwd", "ls", "echo", "run", "spawn", "parallel", "jobs", "history", "clear", "exit",
    ];
    if builtins.contains(&cmd) {
        return true;
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for path in std::env::split_paths(&paths) {
            let exe = path.join(cmd);
            if exe.is_file() {
                return true;
            }
        }
    }
    false
}

/// `NO_COLOR` (https://no-color.org): checked lazily, same convention as
/// `main.rs`'s diagnostics/prompt.
fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some()
}

/// Push `text` styled, unless `plain` (NO_COLOR) — then always default.
/// No-op on empty text so callers don't need to guard slices themselves.
fn push(styled: &mut StyledText, style: Style, text: &str, plain: bool) {
    if text.is_empty() {
        return;
    }
    styled.push((
        if plain { Style::default() } else { style },
        text.to_string(),
    ));
}

/// True when a lex failure is specifically an in-progress (unterminated)
/// string — the overwhelmingly common lex error while typing, since it's
/// true for every keystroke between the opening and closing quote.
fn is_unterminated_string(msg: &str) -> bool {
    msg.contains("unterminated") && msg.contains("string")
}

impl Highlighter for ShoalHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut styled = StyledText::new();
        let lx = Lexer::new(line);
        let mut pos = 0;
        let mut expect_cmd = true;
        let plain = no_color();

        loop {
            let next_pos = lx.skip_trivia(pos);
            if next_pos > pos {
                push(&mut styled, Style::default(), &line[pos..next_pos], plain);
                pos = next_pos;
            }
            if pos >= line.len() {
                break;
            }

            let (tok, span) = match lx.token(pos, Mode::Expr) {
                Ok(t) => t,
                Err(e) => {
                    // Degrade gracefully instead of dumping the remainder in
                    // default style: an unterminated string is the normal
                    // state while typing one (every keystroke between the
                    // open and close quote hits this), so keep it looking
                    // like an in-progress string (yellow) rather than "the
                    // highlighter broke".
                    let style = if is_unterminated_string(&e.msg) {
                        Style::new().fg(Color::Yellow)
                    } else {
                        Style::default()
                    };
                    push(&mut styled, style, &line[pos..], plain);
                    pos = line.len();
                    break;
                }
            };

            let start = span.start as usize;
            let end = span.end as usize;
            if start > pos {
                push(&mut styled, Style::default(), &line[pos..start], plain);
            }
            if let Tok::Eof = tok {
                break;
            }

            // A bare identifier that isn't a keyword/literal/assignment
            // target, sitting where a statement head is expected, is a
            // COMMAND head (TDD §3.1's dispatch rule, approximated the same
            // way `completer::classify` does). Its *own* word boundary must
            // then come from CMD-mode lexing, not the EXPR-mode span above —
            // EXPR mode splits on `.`, so `colorcheck.sh` would otherwise be
            // cut down to `colorcheck` (member-access punctuation) instead of
            // styling as the one bare word the parser will actually run.
            if let Tok::Ident(name) = &tok {
                if is_keyword(name) {
                    push(
                        &mut styled,
                        Style::new().fg(Color::Green).bold(),
                        &line[start..end],
                        plain,
                    );
                    pos = end;
                    expect_cmd = false;
                    continue;
                }
                if matches!(name.as_str(), "true" | "false" | "null") {
                    push(
                        &mut styled,
                        Style::new().fg(Color::LightCyan),
                        &line[start..end],
                        plain,
                    );
                    pos = end;
                    expect_cmd = false;
                    continue;
                }
                let rest = line[end..].trim_start();
                let is_assign = rest.starts_with('=')
                    || rest.starts_with("+=")
                    || rest.starts_with("-=")
                    || rest.starts_with("*=")
                    || rest.starts_with("/=");
                if !is_assign && expect_cmd {
                    let head_end = match lx.token(start, Mode::Cmd) {
                        Ok((_, cmd_span)) => (cmd_span.end as usize).max(end),
                        Err(_) => end,
                    };
                    let head_text = &line[start..head_end];
                    let style = if is_valid_command(head_text) {
                        Style::new().fg(Color::Green)
                    } else {
                        Style::new().fg(Color::Red).bold()
                    };
                    push(&mut styled, style, head_text, plain);
                    pos = highlight_cmd_tail(&lx, line, head_end, &mut styled, plain);
                    // `highlight_cmd_tail` stops at (without consuming) the
                    // next statement boundary; that token's own handling
                    // below re-derives `expect_cmd` uniformly.
                    continue;
                }
                // Assignment target, or a plain identifier reference in expr
                // position (not a command head) — both read the same way.
                push(
                    &mut styled,
                    Style::new().fg(Color::LightBlue),
                    &line[start..end],
                    plain,
                );
                pos = end;
                expect_cmd = false;
                continue;
            }

            let mut next_expect = matches!(tok, Tok::Newline);
            let style = match &tok {
                Tok::Int(_) | Tok::Float(_) | Tok::Size(_) | Tok::Duration(_) => {
                    Style::new().fg(Color::Cyan)
                }
                Tok::Str(_) | Tok::StrInterp(_) => Style::new().fg(Color::Yellow),
                Tok::Regex(_) => Style::new().fg(Color::LightMagenta),
                Tok::DateTime(_) | Tok::Time { .. } => Style::new().fg(Color::LightBlue),
                Tok::Semi | Tok::LBrace | Tok::Pipe | Tok::Caret => {
                    next_expect = true;
                    Style::default()
                }
                Tok::LParen | Tok::LBracket | Tok::RParen | Tok::RBracket | Tok::RBrace => {
                    Style::default().fg(Color::DarkGray)
                }
                Tok::Eq | Tok::PlusEq | Tok::MinusEq | Tok::StarEq | Tok::SlashEq => {
                    Style::new().fg(Color::Red)
                }
                Tok::Plus
                | Tok::Minus
                | Tok::Star
                | Tok::Slash
                | Tok::Percent
                | Tok::EqEq
                | Tok::NotEq
                | Tok::Lt
                | Tok::Le
                | Tok::Gt
                | Tok::Ge
                | Tok::AndAnd
                | Tok::OrOr => Style::new().fg(Color::LightMagenta),
                _ => Style::default(),
            };
            push(&mut styled, style, &line[start..end], plain);
            pos = end;
            expect_cmd = next_expect;
        }
        if pos < line.len() {
            push(&mut styled, Style::default(), &line[pos..], plain);
        }
        styled
    }
}

/// Style the rest of a COMMAND statement (everything after its head word)
/// using real CMD-mode word boundaries instead of always re-tearing it into
/// unrelated EXPR-mode tokens (the bug behind `ls --color=auto` rendering as
/// five separately-colored punctuation tokens, or `colorcheck.sh` rendering
/// as a member-access chain). Flags get one color, paths another
/// (underlined, so they read as filesystem references), globs another;
/// plain words/env-assignments/redirects fall back sensibly. Stops — without
/// consuming — at the first statement boundary (`;`, newline, or EOF) so the
/// caller's keyword/`expect_cmd` bookkeeping stays centralized in one place.
fn highlight_cmd_tail(
    lx: &Lexer,
    line: &str,
    mut pos: usize,
    styled: &mut StyledText,
    plain: bool,
) -> usize {
    loop {
        let next_pos = lx.skip_trivia(pos);
        if next_pos > pos {
            push(styled, Style::default(), &line[pos..next_pos], plain);
            pos = next_pos;
        }
        if pos >= line.len() {
            return pos;
        }
        let (tok, span) = match lx.token(pos, Mode::Cmd) {
            Ok(t) => t,
            Err(e) => {
                let style = if is_unterminated_string(&e.msg) {
                    Style::new().fg(Color::Yellow)
                } else {
                    Style::default()
                };
                push(styled, style, &line[pos..], plain);
                return line.len();
            }
        };
        let start = span.start as usize;
        let end = span.end as usize;
        if start > pos {
            push(styled, Style::default(), &line[pos..start], plain);
        }
        match tok {
            // Statement boundary: hand back to the caller unconsumed so its
            // own Mode::Expr handling (keyword styling, `expect_cmd` reset)
            // applies uniformly, whether we got here via CMD mode or not.
            Tok::Semi | Tok::Newline | Tok::Eof => return start,
            Tok::FlagLong(_)
            | Tok::FlagLongEq(_, _)
            | Tok::FlagLongPendingValue(_)
            | Tok::FlagShort(_)
            | Tok::Dash
            | Tok::DashDash => {
                push(
                    styled,
                    Style::new().fg(Color::Cyan),
                    &line[start..end],
                    plain,
                );
            }
            Tok::PathWord(_) => {
                push(
                    styled,
                    Style::new().fg(Color::LightBlue).underline(),
                    &line[start..end],
                    plain,
                );
            }
            Tok::GlobWord(_) => {
                push(
                    styled,
                    Style::new().fg(Color::LightMagenta),
                    &line[start..end],
                    plain,
                );
            }
            Tok::Str(_) | Tok::StrInterp(_) => {
                push(
                    styled,
                    Style::new().fg(Color::Yellow),
                    &line[start..end],
                    plain,
                );
            }
            Tok::RedirOut | Tok::RedirAppend | Tok::RedirIn | Tok::Amp => {
                push(
                    styled,
                    Style::new().fg(Color::DarkGray),
                    &line[start..end],
                    plain,
                );
            }
            Tok::EnvAssign(_, _) => {
                push(
                    styled,
                    Style::new().fg(Color::LightBlue),
                    &line[start..end],
                    plain,
                );
            }
            _ => {
                push(styled, Style::default(), &line[start..end], plain);
            }
        }
        pos = end;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `NO_COLOR` is process-global state; serialize every test in this
    // module against the one test that mutates it so cargo's parallel test
    // threads can't observe it transiently set (or unset) mid-assertion.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    fn styles_for(line: &str) -> Vec<(Style, String)> {
        let _guard = ENV_GUARD.lock().unwrap();
        ShoalHighlighter.highlight(line, line.len()).buffer
    }

    fn plain_join(spans: &[(Style, String)]) -> String {
        spans.iter().map(|(_, s)| s.as_str()).collect()
    }

    #[test]
    fn round_trips_to_the_original_text() {
        for line in [
            "let x = 1",
            "ls --color=auto",
            "colorcheck.sh",
            "\"unterminated",
            "git checkout -b feat/*",
        ] {
            assert_eq!(plain_join(&styles_for(line)), line);
        }
    }

    #[test]
    fn cmd_mode_flag_is_one_cyan_span() {
        let spans = styles_for("ls --color=auto");
        let flag = spans
            .iter()
            .find(|(_, s)| s == "--color=auto")
            .expect("flag should be a single token, not torn apart");
        assert_eq!(flag.0.foreground, Some(Color::Cyan));
    }

    #[test]
    fn cmd_mode_bare_word_with_dot_is_not_member_access() {
        let spans = styles_for("colorcheck.sh");
        assert!(
            spans.iter().any(|(_, s)| s == "colorcheck.sh"),
            "expected one bare-word span, got {spans:?}"
        );
    }

    #[test]
    fn cmd_mode_path_argument_is_underlined_blue() {
        let spans = styles_for("cat ./foo.txt");
        let path = spans
            .iter()
            .find(|(_, s)| s == "./foo.txt")
            .expect("path should be a single token");
        assert_eq!(path.0.foreground, Some(Color::LightBlue));
        assert!(path.0.is_underline);
    }

    #[test]
    fn cmd_mode_glob_argument_is_magenta() {
        let spans = styles_for("ls *.rs");
        let glob = spans
            .iter()
            .find(|(_, s)| s == "*.rs")
            .expect("glob should be a single token");
        assert_eq!(glob.0.foreground, Some(Color::LightMagenta));
    }

    #[test]
    fn unterminated_string_is_styled_yellow_not_dumped_plain() {
        let spans = styles_for("let x = \"abc");
        let tail = spans
            .iter()
            .find(|(_, s)| s == "\"abc")
            .expect("in-progress string should still be one styled span");
        assert_eq!(tail.0.foreground, Some(Color::Yellow));
    }

    #[test]
    fn no_color_env_forces_every_span_to_default_style() {
        let _guard = ENV_GUARD.lock().unwrap();
        // SAFETY: `ENV_GUARD` above serializes this against every other test
        // in the module that reads styled output, so no other thread can
        // observe `NO_COLOR` transiently set.
        unsafe { std::env::set_var("NO_COLOR", "1") };
        let spans = ShoalHighlighter.highlight("let x = \"abc", 0).buffer;
        unsafe { std::env::remove_var("NO_COLOR") };
        assert!(spans.iter().all(|(style, _)| *style == Style::default()));
    }
}
