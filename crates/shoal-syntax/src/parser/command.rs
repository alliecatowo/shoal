//! COMMAND dispatch and parsing: deciding whether the current position is a
//! command head (`at_command_head`), the `expr_or_command`/`command_stmt`
//! wrappers that use that decision, and the command grammar itself
//! (`command`/`cmd_arg`).

use super::*;

impl<'s> Parser<'s> {
    /// Would the token stream at the current position dispatch as a COMMAND
    /// (used for `&&`/`||` operands and parenthesised command substitution)?
    pub(crate) fn at_command_head(&self) -> ParseResult<bool> {
        let start = self.lx.skip_trivia(self.pos);
        if self.byte(start) == b'^' {
            return Ok(true);
        }
        if self.is_path_head(start) {
            return Ok(true);
        }
        Ok(match self.peek(Mode::Expr) {
            Ok((Tok::Ident(name), s)) => {
                if RESERVED.contains(&name.as_str()) || matches!(name.as_str(), "with" | "spawn") {
                    false
                } else if self.is_interpreter(&name) && self.interp_block_follows(s) {
                    // `tool { … }` / `tool ''' … '''` is an interpreter block
                    // expression, not a command — dispatch EXPR.
                    false
                } else if self.adjacent_postfix_after_ident(s)? {
                    false
                } else if matches!(
                    self.lx.token(s.end as usize, Mode::Expr),
                    Ok((Tok::FatArrow, _))
                ) {
                    // `x => …` is the bare single-param lambda shorthand
                    // (site/content/internals/language-conformance-contract.md primary alternative), never a zero-arg
                    // command named `x` — dispatch EXPR so `(x => x + 1)`
                    // (grouped, or immediately called) parses as a lambda
                    // instead of `x` with argv `["=", ">", "x", "+", "1"]`.
                    false
                } else {
                    // value-bound → EXPR (Var); cmd-bound or unbound → command.
                    !self.bound(&name)
                }
            }
            _ => false,
        })
    }
    /// Parse a command operand (`Expr::Cmd`) when the head dispatches CMD,
    /// otherwise a normal expression. Used for `&&`/`||` operands and inside
    /// `(` … `)` group / command-substitution positions.
    pub(crate) fn expr_or_command(&mut self, min: u8) -> ParseResult<Expr> {
        if self.at_command_head()? {
            let call = self.command()?;
            let span = call.span;
            let e = Expr::Cmd {
                call: Box::new(call),
                span,
            };
            self.expr_tail(e, min)
        } else {
            self.expr(min)
        }
    }
    /// Dispatch and parse a COMMAND statement, then absorb any trailing
    /// `&&`/`||` command/expr operands (eval-audit #6).
    pub(crate) fn command_stmt(&mut self) -> ParseResult<Stmt> {
        let call = self.command()?;
        let cspan = call.span;
        let e = Expr::Cmd {
            call: Box::new(call),
            span: cspan,
        };
        let e = self.expr_tail(e, 0)?;
        let span = e.span();
        Ok(Stmt::Expr { expr: e, span })
    }

    pub(crate) fn command(&mut self) -> ParseResult<CmdCall> {
        let start = self.lx.skip_trivia(self.pos);
        let mut env_prefix = vec![];
        loop {
            match self.peek(Mode::Cmd)? {
                (Tok::EnvAssign(name, val), s) => {
                    self.bump(Mode::Cmd)?;
                    let value = CmdArg::Word { text: val, span: s };
                    env_prefix.push(EnvPrefix {
                        name,
                        value,
                        span: s,
                    })
                }
                _ => break,
            }
        }
        let forced = self.eat(Mode::Cmd, &Tok::Caret)?.is_some();
        // A path literal (`./x.sh`, `/bin/ls`, `~/x`) is a valid command head.
        let (head, _) = match self.bump(Mode::Cmd)? {
            (Tok::Word(x), s) | (Tok::PathWord(x), s) => (x, s),
            (x, s) => {
                return Err(ParseError::new(
                    format!("expected command head, found {x:?}"),
                    s,
                ));
            }
        };
        let mut args = vec![];
        let mut redirects: Vec<Redirect> = vec![];
        let mut background = false;
        let mut trailing = None;
        loop {
            let (t, s) = self.peek(Mode::Cmd)?;
            // `git log.len()` / `ls somedir.len()` / `git log.where(…)`: a
            // command ARGUMENT word carrying a `.` (lexed as one Cmd-mode
            // word — Cmd words don't break on `.`) immediately glued to `(`
            // is almost always an attempt to chain a method onto the
            // command's result. That grammar is ambiguous and not supported
            // (the receiver would be the whole command, not one word) — but
            // when the parenthesised content itself then fails to parse
            // (empty `()`, or a bare `.field` shorthand that isn't a whole
            // call argument), point at the actual fix: wrap the command.
            let paren_after_dotted_arg = matches!(t, Tok::LParen)
                && matches!(
                    args.last(),
                    Some(CmdArg::Word { text, span }) if span.end == s.start && text.contains('.')
                );
            match t {
                Tok::Newline
                | Tok::Semi
                | Tok::Eof
                | Tok::RBrace
                | Tok::RParen
                | Tok::AndAnd
                | Tok::OrOr => break,
                Tok::Pipe => {
                    return Err(ParseError::new("shoal has no pipe operator", s).hint(
                        "data composes with `.` (try `ls.where(.size > 1mb)`); raw byte plumbing \
                         is `.feed(cmd)`; verbatim POSIX lives in `sh { … }`",
                    ));
                }
                Tok::Amp => {
                    let (_, amp_s) = self.bump(Mode::Cmd)?;
                    // `&>` / `&>>` — the box-era stream-merging redirect
                    // (site/content/internals/values-streams-execution.md). Without this check it silently reparses as
                    // `(cmd &) > f`, a backgrounded command compared to a
                    // variable.
                    if let Ok((Tok::RedirOut | Tok::RedirAppend, s2)) =
                        self.lx.token(amp_s.end as usize, Mode::Cmd)
                        && s2.start == amp_s.end
                    {
                        return Err(ParseError::new(
                            "shoal has no stream-merging redirect",
                            Span::new(amp_s.start as usize, s2.end as usize),
                        )
                        .hint(
                            "capture is structured: `(cmd).out` / `(cmd).stderr`; a \
                             statement-position PTY run already merges the streams",
                        ));
                    }
                    background = true;
                    break;
                }
                Tok::LBrace => {
                    trailing = Some(self.block()?);
                    break;
                }
                Tok::RedirIn => {
                    // `<<` (heredoc) / `<<<` (here-string) — box-era spellings
                    // with curated teaching errors (site/content/internals/values-streams-execution.md).
                    if let Ok((Tok::RedirIn, s2)) = self.lx.token(s.end as usize, Mode::Cmd)
                        && s2.start == s.end
                    {
                        let third = matches!(
                            self.lx.token(s2.end as usize, Mode::Cmd),
                            Ok((Tok::RedirIn, s3)) if s3.start == s2.end
                        );
                        return Err(if third {
                            ParseError::new(
                                "shoal has no here-strings",
                                Span::new(s.start as usize, s2.end as usize + 1),
                            )
                            .hint("feed the value instead: `\"text\".feed(cmd)`")
                        } else {
                            ParseError::new(
                                "shoal has no heredocs",
                                Span::new(s.start as usize, s2.end as usize),
                            )
                            .hint(
                                "feed a string or multiline literal instead: \
                                 `value.feed(cmd)`, or use an interpreter block: \
                                 `python { … }`",
                            )
                        });
                    }
                    if redirects
                        .iter()
                        .any(|redirect| redirect.kind == RedirectKind::In)
                    {
                        return Err(ParseError::new(
                            "a command may have only one stdin redirect",
                            s,
                        )
                        .hint(
                            "choose one input source; compose typed input first and use `.feed(cmd)` when needed",
                        ));
                    }
                    self.bump(Mode::Cmd)?;
                    let target = self.cmd_arg()?;
                    redirects.push(Redirect {
                        kind: RedirectKind::In,
                        span: Span::new(s.start as usize, target.span().end as usize),
                        target,
                    });
                }
                Tok::RedirOut | Tok::RedirAppend => {
                    // `2>` / `1>>` — fd-numbered redirects (site/content/internals/values-streams-execution.md): a bare
                    // digit word glued to the redirect. Without this check the
                    // digit silently passes as an ARGUMENT and the redirect
                    // grabs stdout — the opposite of the user's intent.
                    if let Some(CmdArg::Word { text, span }) = args.last()
                        && !text.is_empty()
                        && text.bytes().all(|b| b.is_ascii_digit())
                        && span.end == s.start
                    {
                        return Err(ParseError::new(
                            "shoal has no fd-numbered redirects",
                            Span::new(span.start as usize, s.end as usize),
                        )
                        .hint(
                            "stderr is structured — `(cmd).stderr`, or \
                             `try { cmd } catch e { e.stderr }`; a statement-position \
                             PTY run already merges the streams",
                        ));
                    }
                    if redirects.iter().any(|redirect| {
                        matches!(redirect.kind, RedirectKind::Out | RedirectKind::Append)
                    }) {
                        return Err(ParseError::new(
                            "a command may have only one stdout redirect",
                            s,
                        )
                        .hint(
                            "choose one target; for fan-out, capture `(cmd).out` and handle each write explicitly",
                        ));
                    }
                    let kind = match self.bump(Mode::Cmd)?.0 {
                        Tok::RedirAppend => RedirectKind::Append,
                        _ => RedirectKind::Out,
                    };
                    let target = self.cmd_arg()?;
                    redirects.push(Redirect {
                        kind,
                        span: Span::new(s.start as usize, target.span().end as usize),
                        target,
                    });
                }
                _ => match self.cmd_arg() {
                    Ok(arg) => args.push(arg),
                    Err(e) if paren_after_dotted_arg => {
                        return Err(e.hint(
                            "to chain methods on a command, wrap it in parentheses — \
                             (git log).len()",
                        ));
                    }
                    Err(e) => return Err(e),
                },
            }
        }
        Ok(CmdCall {
            head,
            forced,
            args,
            redirects,
            env_prefix,
            background,
            trailing,
            span: Span::new(start, self.pos),
        })
    }
    pub(crate) fn cmd_arg(&mut self) -> ParseResult<CmdArg> {
        let (t, s) = self.bump(Mode::Cmd)?;
        Ok(match t {
            Tok::Word(text) => CmdArg::Word { text, span: s },
            // `IDENT=rest` is an env-prefix only AT HEAD POSITION (site/content/internals/language-conformance-contract.md —
            // the lexer classifies by shape and "the parser decides"). As an
            // argument it is a plain word: `echo FOO=bar`, `make CC=gcc`.
            Tok::EnvAssign(name, rest) => CmdArg::Word {
                text: format!("{name}={rest}"),
                span: s,
            },
            Tok::PathWord(text) => CmdArg::Path { text, span: s },
            Tok::GlobWord(pattern) => CmdArg::Glob { pattern, span: s },
            Tok::Str(x) => CmdArg::Str {
                expr: Expr::Str { value: x, span: s },
                span: s,
            },
            Tok::StrInterp(x) => CmdArg::Str {
                expr: self.interp(x, s)?,
                span: s,
            },
            Tok::LParen => {
                let e = self.expr_or_command(0)?;
                self.expect(Mode::Expr, Tok::RParen, "`)`")?;
                CmdArg::Expr {
                    expr: e,
                    span: Span::new(s.start as usize, self.pos),
                }
            }
            Tok::FlagLong(name) => CmdArg::FlagLong {
                name,
                value: None,
                span: s,
            },
            Tok::FlagLongEq(name, v) => CmdArg::FlagLong {
                name,
                value: Some(Box::new(CmdArg::Word { text: v, span: s })),
                span: s,
            },
            Tok::FlagLongPendingValue(name) => {
                let v = self.cmd_arg()?;
                CmdArg::FlagLong {
                    name,
                    value: Some(Box::new(v)),
                    span: Span::new(s.start as usize, self.pos),
                }
            }
            Tok::FlagShort(chars) => CmdArg::FlagShort { chars, span: s },
            Tok::DashDash => CmdArg::DashDash { span: s },
            Tok::Dash => CmdArg::Dash { span: s },
            _ => return Err(ParseError::new("expected command argument", s)),
        })
    }
}
