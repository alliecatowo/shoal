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
                } else if INTERPRETERS.contains(&name.as_str()) && self.interp_block_follows(s) {
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
                    // (TDD §3.2 primary alternative), never a zero-arg
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
        let mut redirects = vec![];
        let mut background = false;
        let mut trailing = None;
        loop {
            let (t, s) = self.peek(Mode::Cmd)?;
            match t{Tok::Newline|Tok::Semi|Tok::Eof|Tok::RBrace|Tok::RParen|Tok::AndAnd|Tok::OrOr=>break,Tok::Pipe=>return Err(ParseError::new("shoal has no pipe operator",s).hint("data composes with `.` (try `ls.where(.size > 1mb)`); raw byte plumbing is `.feed(cmd)`; verbatim POSIX lives in `sh { … }`")),Tok::Amp=>{self.bump(Mode::Cmd)?;background=true;break},Tok::LBrace=>{trailing=Some(self.block()?);break},Tok::RedirOut|Tok::RedirAppend|Tok::RedirIn=>{let kind=match self.bump(Mode::Cmd)?.0{Tok::RedirOut=>RedirectKind::Out,Tok::RedirAppend=>RedirectKind::Append,_=>RedirectKind::In};let target=self.cmd_arg()?;redirects.push(Redirect{kind,span:Span::new(s.start as usize,target.span().end as usize),target})},_=>args.push(self.cmd_arg()?)}
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
