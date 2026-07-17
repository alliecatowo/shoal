use nu_ansi_term::{Color, Style};
use reedline::{Highlighter, StyledText};
use shoal_syntax::commands::{CommandFacts, CommandSource, builtin_names, resolve_command_source};
use shoal_syntax::{Lexer, Mode, Tok};
use shoal_value::{Env, Value};

/// Live syntax highlighter. Holds a shared handle to the session [`Env`]
/// (`Env` clones are `Arc`-backed, same as the completer's) so statement
/// dispatch is approximated with the parser's real rule: a value-bound head
/// is a variable reference, not an unknown command to paint red.
#[derive(Default)]
pub struct ShoalHighlighter {
    env: Option<Env>,
}

impl ShoalHighlighter {
    pub fn with_env(env: Env) -> Self {
        ShoalHighlighter { env: Some(env) }
    }

    /// The session binding for `name`, if any.
    fn binding(&self, name: &str) -> Option<Value> {
        self.env.as_ref().and_then(|e| e.get(name))
    }
}

fn is_keyword(name: &str) -> bool {
    // Statement-leading keywords: the parser's reserved set plus `with`/`spawn`,
    // which it special-cases identically (parser/command.rs). `true`/`false`/
    // `null` are handled separately as literals, so they are excluded here.
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
            | "with"
            | "spawn"
    )
}

fn is_valid_command(cmd: &str) -> bool {
    // Builtin command heads come from shoal-eval's canonical registry (no more
    // hand-maintained list drift — the old local array carried a bogus `clear`
    // and missed most real heads). External commands still resolve via PATH.
    if builtin_names().binary_search(&cmd).is_ok() {
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

/// Does the raw text at `pos` begin a path literal (`./ ../ ~ ~/ /…`)? A
/// byte-level replica of the parser's `is_path_head` (site/content/internals/language-conformance-contract.md): such a head
/// dispatches CMD, so the highlighter must not lex it as EXPR punctuation.
fn is_path_head_bytes(bytes: &[u8], pos: usize) -> bool {
    let at = |i: usize| bytes.get(i).copied().unwrap_or(0);
    match at(pos) {
        b'/' => true,
        b'~' => matches!(at(pos + 1), 0 | b'/' | b' ' | b'\t' | b'\r' | b'\n' | b';'),
        b'.' => at(pos + 1) == b'/' || (at(pos + 1) == b'.' && at(pos + 2) == b'/'),
        _ => false,
    }
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

            // A path-shaped statement head (`./x.sh`, `/bin/ls`, `~/t`, `../r`)
            // dispatches CMD in the parser (`is_path_head`) — lex it as one
            // CMD-mode path word instead of tearing it into EXPR punctuation.
            if expect_cmd
                && is_path_head_bytes(line.as_bytes(), pos)
                && let Ok((Tok::PathWord(_), s)) = lx.token(pos, Mode::Cmd)
            {
                let end = (s.end as usize).max(pos + 1);
                push(
                    &mut styled,
                    Style::new().fg(Color::LightBlue).underline(),
                    &line[pos..end],
                    plain,
                );
                pos = highlight_cmd_tail(&lx, line, end, &mut styled, plain);
                continue;
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
            // COMMAND head (site/content/internals/language-conformance-contract.md dispatch rule, approximated the same
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
                // Mirror the parser's statement dispatch (site/content/internals/language-conformance-contract.md): a
                // VALUE-bound head is an EXPR variable reference, and an
                // ident immediately followed by `.ident` (no whitespace) is
                // the invoke-then-chain refinement — both dispatch EXPR, so
                // neither may be judged as a command head. A CALLABLE binding
                // (session `fn`/alias) dispatches CMD and is a known-valid
                // command even though PATH has never heard of it.
                let bound = self.binding(name);
                let source = resolve_command_source(
                    name,
                    CommandFacts {
                        session_callable: bound.as_ref().is_some_and(Value::is_callable),
                        session_value: bound.as_ref().is_some_and(|value| !value.is_callable()),
                        value_eligible: rest.is_empty()
                            || rest.starts_with(';')
                            || rest.starts_with('\n'),
                        forced: false,
                        // The highlighter has no adapter catalog; known adapter
                        // names are still supplied by completion.
                        adapter: false,
                    },
                );
                let callable = source == CommandSource::SessionCallable;
                let value_bound = source == CommandSource::BoundValue;
                let chains = line[end..].starts_with('.')
                    && line.as_bytes()[end + 1..]
                        .first()
                        .is_some_and(|b| b.is_ascii_alphabetic() || *b == b'_');
                if !is_assign && expect_cmd && !value_bound && !chains {
                    let head_end = match lx.token(start, Mode::Cmd) {
                        Ok((_, cmd_span)) => (cmd_span.end as usize).max(end),
                        Err(_) => end,
                    };
                    let head_text = &line[start..head_end];
                    let style = if callable || is_valid_command(head_text) {
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
                // Invoke-then-chain head: `ls` in `ls.where(…)` still names a
                // command — color it by resolvability, but keep EXPR token
                // boundaries for the chain that follows.
                let style = if expect_cmd && chains && !value_bound {
                    if callable || is_valid_command(name) {
                        Style::new().fg(Color::Green)
                    } else {
                        Style::new().fg(Color::Red).bold()
                    }
                } else {
                    // Assignment target, bound-variable head, or a plain
                    // identifier reference in expr position — all read the
                    // same way.
                    Style::new().fg(Color::LightBlue)
                };
                push(&mut styled, style, &line[start..end], plain);
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

    // `NO_COLOR`/env is process-global; every env-touching test across this
    // bin shares `crate::ENV_TEST_LOCK` (defined in main.rs) so a setter in one
    // module can't leak env state into another module's color assertion.
    //
    // Almost every test below asserts specific ANSI colors/styles, so the
    // color environment must be forced on regardless of whatever ambient
    // `NO_COLOR` the invoking shell/CI happens to export (deep audit H13):
    // the product is right to honor `NO_COLOR`, but these tests exercise the
    // *colored* branch of `highlight()` on purpose and must not silently
    // degrade to asserting nothing whenever `NO_COLOR=1` is set in the test
    // runner's environment. `with_forced_color` unsets `NO_COLOR` for the
    // duration of the closure (still under `ENV_TEST_LOCK`) and restores
    // whatever was there before, so this suite passes identically whether
    // invoked as `NO_COLOR=1 cargo test` or with `NO_COLOR` unset.
    fn with_forced_color<T>(f: impl FnOnce() -> T) -> T {
        let _guard = crate::ENV_TEST_LOCK.lock().unwrap();
        let prev = std::env::var_os("NO_COLOR");
        // SAFETY: serialized by `ENV_TEST_LOCK` against every other test in
        // this binary that reads or writes `NO_COLOR`.
        unsafe { std::env::remove_var("NO_COLOR") };
        let result = f();
        match prev {
            // SAFETY: same lock/serialization as above.
            Some(v) => unsafe { std::env::set_var("NO_COLOR", v) },
            None => unsafe { std::env::remove_var("NO_COLOR") },
        }
        result
    }

    fn styles_for(line: &str) -> Vec<(Style, String)> {
        with_forced_color(|| {
            ShoalHighlighter::default()
                .highlight(line, line.len())
                .buffer
        })
    }

    /// Highlight with a session env carrying one value binding (`someVar`)
    /// and one callable binding (`deploy`).
    fn styles_with_bindings(line: &str) -> Vec<(Style, String)> {
        with_forced_color(|| styles_with_bindings_inner(line))
    }

    fn styles_with_bindings_inner(line: &str) -> Vec<(Style, String)> {
        let env = Env::root();
        env.declare("someVar", Value::Int(42), false);
        env.declare(
            "deploy",
            Value::CmdRef(std::sync::Arc::new(shoal_ast::CmdCall {
                head: "deploy".into(),
                forced: false,
                args: vec![],
                redirects: vec![],
                env_prefix: vec![],
                background: false,
                trailing: None,
                span: shoal_ast::Span::new(0, 0),
            })),
            false,
        );
        ShoalHighlighter::with_env(env)
            .highlight(line, line.len())
            .buffer
    }

    #[test]
    fn bound_variable_at_statement_head_is_not_an_unknown_command() {
        // Regression: `let someVar = 6 * 7` then typing `someVar` painted it
        // red-bold (unknown command). A value-bound head dispatches EXPR — it
        // must read as a variable reference.
        let spans = styles_with_bindings("someVar");
        let head = spans
            .iter()
            .find(|(_, s)| s == "someVar")
            .expect("identifier span");
        assert_eq!(
            head.0.foreground,
            Some(Color::LightBlue),
            "bound variable must style as a reference, got {spans:?}"
        );
    }

    #[test]
    fn ineligible_value_shadow_falls_through_to_builtin_highlighting() {
        let spans = with_forced_color(|| {
            let env = Env::root();
            env.declare("ls", Value::Int(42), false);
            ShoalHighlighter::with_env(env).highlight("ls .", 4).buffer
        });
        let head = spans
            .iter()
            .find(|(_, text)| text == "ls")
            .expect("head span");
        assert_eq!(head.0.foreground, Some(Color::Green), "got {spans:?}");
    }

    #[test]
    fn callable_binding_at_statement_head_is_a_valid_command() {
        // A session `fn` is a command binding: green, never red, even though
        // PATH has never heard of it.
        let spans = styles_with_bindings("deploy staging");
        let head = spans
            .iter()
            .find(|(_, s)| s == "deploy")
            .expect("head span");
        assert_eq!(head.0.foreground, Some(Color::Green), "got {spans:?}");
    }

    #[test]
    fn registry_builtin_head_is_valid_without_path() {
        // `undo`/`reef`/`dirs`/`jobs` are shoal builtins with no PATH binary —
        // they must still highlight green, proving the highlighter reads the
        // eval registry (which is consulted before the PATH fallback) rather
        // than a stale local list. `clear`, conversely, is gone from the
        // registry (it was never a real builtin).
        for head in ["undo", "reef", "dirs", "jobs"] {
            let spans = styles_for(head);
            let h = spans
                .iter()
                .find(|(_, s)| s == head)
                .unwrap_or_else(|| panic!("{head}: no head span in {spans:?}"));
            assert_eq!(h.0.foreground, Some(Color::Green), "{head}: {spans:?}");
        }
    }

    #[test]
    fn spawn_and_with_style_as_keywords() {
        // `spawn`/`with` are statement keywords (the parser special-cases them
        // alongside RESERVED) — green + bold, not resolved-via-PATH command
        // heads. Regression guard: dropping them from the highlighter's old
        // valid-command list must not make them flag red.
        for kw in ["spawn", "with"] {
            let spans = styles_for(kw);
            let s = spans
                .iter()
                .find(|(_, t)| t == kw)
                .unwrap_or_else(|| panic!("{kw}: no span in {spans:?}"));
            assert_eq!(s.0.foreground, Some(Color::Green), "{kw}: {spans:?}");
            assert!(s.0.is_bold, "{kw} should be a bold keyword: {spans:?}");
        }
    }

    #[test]
    fn unbound_head_still_flags_red() {
        let spans = styles_with_bindings("qzxunknowncmd");
        let head = spans
            .iter()
            .find(|(_, s)| s == "qzxunknowncmd")
            .expect("head span");
        assert_eq!(head.0.foreground, Some(Color::Red), "got {spans:?}");
    }

    #[test]
    fn invoke_then_chain_head_keeps_expr_boundaries() {
        // `ls.where(...)` dispatches EXPR (invoke-then-chain): the head must
        // not be torn into one giant red CMD word `ls.where(.size`.
        let spans = styles_for("ls.where(.size > 1mb)");
        assert!(
            spans.iter().any(|(_, s)| s == "ls"),
            "head should be its own span, got {spans:?}"
        );
        let ls = spans.iter().find(|(_, s)| s == "ls").unwrap();
        assert_eq!(
            ls.0.foreground,
            Some(Color::Green),
            "`ls` resolves on PATH/builtins, got {spans:?}"
        );
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
    fn bare_ident_dot_chain_follows_parser_dispatch() {
        // The parser dispatches `colorcheck.sh` as EXPR (invoke-then-chain:
        // `colorcheck().sh` — empirically `undefined_var: colorcheck`), NOT
        // as one command word; running a script by name needs the path form
        // `./colorcheck.sh`. The highlighter must mirror that dispatch: the
        // head is its own span, red because nothing resolves it. (This test
        // previously asserted the opposite — one bare-word CMD span — which
        // contradicted the parser.)
        let spans = styles_for("colorcheck.sh");
        let head = spans
            .iter()
            .find(|(_, s)| s == "colorcheck")
            .expect("head should be its own span");
        assert_eq!(head.0.foreground, Some(Color::Red), "got {spans:?}");
        // The path form IS one underlined command word.
        let spans = styles_for("./colorcheck.sh");
        assert!(
            spans.iter().any(|(_, s)| s == "./colorcheck.sh"),
            "path head should stay one span, got {spans:?}"
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
        let _guard = crate::ENV_TEST_LOCK.lock().unwrap();
        // SAFETY: `ENV_GUARD` above serializes this against every other test
        // in the module that reads styled output, so no other thread can
        // observe `NO_COLOR` transiently set.
        unsafe { std::env::set_var("NO_COLOR", "1") };
        let spans = ShoalHighlighter::default()
            .highlight("let x = \"abc", 0)
            .buffer;
        unsafe { std::env::remove_var("NO_COLOR") };
        assert!(spans.iter().all(|(style, _)| *style == Style::default()));
    }
}
