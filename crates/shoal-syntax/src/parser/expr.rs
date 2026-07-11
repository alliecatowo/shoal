//! Expression parsing: the Pratt-style binary-operator loop
//! (`expr`/`expr_tail`), unary/primary/postfix, call-argument lists, and
//! string interpolation.

use super::*;

impl<'s> Parser<'s> {
    pub(crate) fn expr(&mut self, min: u8) -> ParseResult<Expr> {
        let lhs = self.unary()?;
        self.expr_tail(lhs, min)
    }
    pub(crate) fn expr_tail(&mut self, mut lhs: Expr, min: u8) -> ParseResult<Expr> {
        loop {
            let (mut t, _) = self.peek(Mode::Expr)?;
            // Cross-newline continuation (§2.1): a trailing binary operator (the
            // newline is already inside an open subexpression) never reaches
            // here, but a `catch` on the *next* line must still attach.
            if min == 0 && matches!(t, Tok::Newline) {
                if self.continue_if(|t| matches!(t, Tok::Ident(x) if x == "catch"))? {
                    t = self.peek(Mode::Expr)?.0;
                } else {
                    break;
                }
            }
            if min == 0 && matches!(&t, Tok::Ident(x) if x == "catch") {
                self.bump(Mode::Expr)?;
                let binder = if let (Tok::Ident(name), _) = self.peek(Mode::Expr)? {
                    self.bump(Mode::Expr)?;
                    Some(name)
                } else {
                    None
                };
                let handler = if matches!(self.peek(Mode::Expr)?.0, Tok::LBrace) {
                    let block = self.block()?;
                    Expr::Block {
                        span: block.span,
                        block,
                    }
                } else {
                    self.expr(0)?
                };
                let span = Span::new(lhs.span().start as usize, handler.span().end as usize);
                lhs = Expr::Catch {
                    expr: Box::new(lhs),
                    binder,
                    handler: Box::new(handler),
                    span,
                };
                continue;
            }
            let Some((bp, op)) = binop(&t) else { break };
            if bp < min {
                break;
            }
            if is_comparison_token(&t)
                && matches!(
                    lhs,
                    Expr::Binary {
                        op: BinOp::Eq
                            | BinOp::Ne
                            | BinOp::Lt
                            | BinOp::Le
                            | BinOp::Gt
                            | BinOp::Ge
                            | BinOp::In,
                        ..
                    }
                )
            {
                return Err(ParseError::new(
                    "comparison operators do not chain",
                    self.peek(Mode::Expr)?.1,
                )
                .hint("combine comparisons explicitly with `&&`"));
            }
            self.bump(Mode::Expr)?;
            // A trailing binary operator continues the statement across newlines
            // (§2.1); `&&`/`||` operands may be commands (eval-audit #6).
            self.skip_newlines()?;
            let rhs = if matches!(op, Some(BinOp::And) | Some(BinOp::Or)) {
                self.expr_or_command(bp + 1)?
            } else {
                self.expr(bp + 1)?
            };
            let span = Span::new(lhs.span().start as usize, rhs.span().end as usize);
            lhs = if matches!(t, Tok::DotDot | Tok::DotDotEq) {
                Expr::Range {
                    start: Box::new(lhs),
                    end: Box::new(rhs),
                    inclusive: matches!(t, Tok::DotDotEq),
                    span,
                }
            } else {
                Expr::Binary {
                    op: op.unwrap(),
                    lhs: Box::new(lhs),
                    rhs: Box::new(rhs),
                    span,
                }
            };
        }
        Ok(lhs)
    }
    pub(crate) fn unary(&mut self) -> ParseResult<Expr> {
        let (t, s) = self.peek(Mode::Expr)?;
        if matches!(t, Tok::Bang | Tok::Minus) {
            self.bump(Mode::Expr)?;
            let e = self.unary()?;
            let end = e.span().end;
            return Ok(Expr::Unary {
                op: if matches!(t, Tok::Bang) {
                    UnOp::Not
                } else {
                    UnOp::Neg
                },
                expr: Box::new(e),
                span: Span::new(s.start as usize, end as usize),
            });
        }
        let p = self.primary()?;
        self.postfix(p)
    }
    pub(crate) fn primary(&mut self) -> ParseResult<Expr> {
        let (t, s) = self.bump(Mode::Expr)?;
        Ok(match t{Tok::Int(value)=>Expr::Int{value,span:s},Tok::Float(value)=>Expr::Float{value,span:s},Tok::Size(bytes)=>Expr::Size{bytes,span:s},Tok::Duration(ns)=>Expr::Duration{ns,span:s},Tok::Time{hour,min,sec}=>Expr::Time{hour,min,sec,span:s},Tok::Str(value)=>Expr::Str{value,span:s},Tok::StrInterp(parts)=>self.interp(parts,s)?,Tok::Regex(src)=>Expr::Regex{src,span:s},Tok::DateTime(iso)=>Expr::DateTime{iso,span:s},Tok::Ident(x)if x=="true"||x=="false"=>Expr::Bool{value:x=="true",span:s},Tok::Ident(x)if x=="null"=>Expr::Null{span:s},Tok::Ident(x)if x=="if"=>return self.if_expr(s.start as usize),Tok::Ident(x)if x=="try"=>return self.try_expr(s.start as usize),Tok::Ident(x)if x=="match"=>return self.match_expr(s.start as usize),Tok::Ident(x)if x=="with"=>return self.with_expr(s.start as usize),Tok::Ident(x)if x=="spawn"=>{let body=self.block()?;Expr::Spawn{body,span:Span::new(s.start as usize,self.pos)}},Tok::Ident(ref x)if INTERPRETERS.contains(&x.as_str())&&self.interp_block_follows(s)=>{let tool=x.clone();if self.byte(self.pos)==b'\''{let(rt,rs)=self.bump(Mode::Expr)?;match rt{Tok::Str(src)=>Expr::LangBlock{tool,src,span:Span::new(s.start as usize,rs.end as usize)},_=>return Err(ParseError::new(format!("expected {tool} payload after `{tool}'`"),rs))}}else{let open=self.expect(Mode::Expr,Tok::LBrace,"`{` or `'''…'''`")?;let(src,end)=self.lx.raw_brace_block(open.start as usize)?;self.pos=end;Expr::LangBlock{tool,src,span:Span::new(s.start as usize,end as usize)}}},Tok::Ident(name)=>{if matches!(self.peek(Mode::Expr)?.0,Tok::FatArrow){self.bump(Mode::Expr)?;let body=self.expr(0)?;let end=body.span().end;Expr::Lambda{params:vec![Param{name,ty:None,default:None,span:s}],body:Box::new(body),span:Span::new(s.start as usize,end as usize)}}else{if !self.repl&&matches!(name.as_str(),"it"|"out"){return Err(ParseError::new(format!("`{name}` is REPL-only"),s).hint("bind a variable to reuse a previous result"))}Expr::Var{name,span:s}}},Tok::LParen=>return self.paren_or_lambda(s.start as usize),Tok::LBracket=>{let mut items=vec![];self.skip_newlines()?;if self.eat(Mode::Expr,&Tok::RBracket)?.is_none(){loop{items.push(self.expr(0)?);self.skip_newlines()?;if self.eat(Mode::Expr,&Tok::Comma)?.is_none(){self.expect(Mode::Expr,Tok::RBracket,"`]`")?;break}self.skip_newlines()?;if self.eat(Mode::Expr,&Tok::RBracket)?.is_some(){break}}}Expr::List{items,span:Span::new(s.start as usize,self.pos)}},Tok::LBrace=>return self.record_or_block(s.start as usize),Tok::Pipe=>return Err(ParseError::new("shoal has no pipe operator",s).hint("data composes with `.`; raw byte plumbing is `.feed(cmd)`; verbatim POSIX lives in `sh { … }`")),_=>return Err(ParseError::new(format!("expected expression, found {t:?}"),s))})
    }
    pub(crate) fn postfix(&mut self, mut e: Expr) -> ParseResult<Expr> {
        loop {
            let (t, _) = self.peek(Mode::Expr)?;
            match t {
                // Leading-`.` on the next line continues this postfix chain
                // (§2.1). A `[`/`(` on the next line does *not* continue.
                Tok::Newline => {
                    if self.continue_if(|t| matches!(t, Tok::Dot | Tok::QuestionDot))? {
                        continue;
                    }
                    break;
                }
                Tok::Dot | Tok::QuestionDot => {
                    let optional = matches!(self.bump(Mode::Expr)?.0, Tok::QuestionDot);
                    let (name, _) = self.ident()?;
                    if self.eat(Mode::Expr, &Tok::LParen)?.is_some() {
                        let mut args = self.args_after_open()?;
                        // Trailing block after a method call (§3.4 `f(a){…}`).
                        if matches!(self.peek(Mode::Expr)?.0, Tok::LBrace) {
                            args.pos.push(self.trailing_block_lambda()?);
                        }
                        let span = Span::new(e.span().start as usize, self.pos);
                        e = Expr::MethodCall {
                            recv: Box::new(e),
                            name,
                            args,
                            optional,
                            span,
                        }
                    } else if !optional && matches!(self.peek(Mode::Expr)?.0, Tok::LBrace) {
                        // `xs.each { … }` — method call with only a thunk arg.
                        let mut args = Args::empty();
                        args.pos.push(self.trailing_block_lambda()?);
                        let span = Span::new(e.span().start as usize, self.pos);
                        e = Expr::MethodCall {
                            recv: Box::new(e),
                            name,
                            args,
                            optional,
                            span,
                        }
                    } else {
                        let span = Span::new(e.span().start as usize, self.pos);
                        e = Expr::Field {
                            recv: Box::new(e),
                            name,
                            optional,
                            span,
                        }
                    }
                }
                Tok::LBracket => {
                    self.bump(Mode::Expr)?;
                    let i = self.expr(0)?;
                    self.expect(Mode::Expr, Tok::RBracket, "`]`")?;
                    let span = Span::new(e.span().start as usize, self.pos);
                    e = Expr::Index {
                        recv: Box::new(e),
                        index: Box::new(i),
                        span,
                    }
                }
                Tok::LParen => {
                    self.bump(Mode::Expr)?;
                    let mut args = self.args_after_open()?;
                    // Trailing block after a call (§3.4 `f(a){…}`).
                    if matches!(self.peek(Mode::Expr)?.0, Tok::LBrace) {
                        args.pos.push(self.trailing_block_lambda()?);
                    }
                    let span = Span::new(e.span().start as usize, self.pos);
                    match e {
                        Expr::Var { name, .. } => e = Expr::FnCall { name, args, span },
                        _ => {
                            return Err(ParseError::new(
                                "only a named function can be called directly",
                                span,
                            ));
                        }
                    }
                }
                _ => break,
            }
        }
        Ok(e)
    }
    /// Parse a trailing `{ … }` block as a zero-argument lambda thunk
    /// (`() => { … }`), per the §3.4 `f(a){…}` desugar.
    pub(crate) fn trailing_block_lambda(&mut self) -> ParseResult<Expr> {
        let block = self.block()?;
        let span = block.span;
        Ok(Expr::Lambda {
            params: vec![],
            body: Box::new(Expr::Block { block, span }),
            span,
        })
    }
    pub(crate) fn args_after_open(&mut self) -> ParseResult<Args> {
        let mut a = Args::empty();
        self.skip_newlines()?;
        if self.eat(Mode::Expr, &Tok::RParen)?.is_some() {
            return Ok(a);
        }
        loop {
            if let (Tok::Dot, dot) = self.peek(Mode::Expr)? {
                let seed = Expr::Var {
                    name: "__item".into(),
                    span: dot,
                };
                let chained = self.postfix(seed)?;
                let body = self.expr_tail(chained, 0)?;
                let end = body.span().end;
                a.pos.push(Expr::Lambda {
                    params: vec![Param {
                        name: "__item".into(),
                        ty: None,
                        default: None,
                        span: dot,
                    }],
                    body: Box::new(body),
                    span: Span::new(dot.start as usize, end as usize),
                });
            } else {
                let save = self.pos;
                if let (Tok::Ident(n), s) = self.peek(Mode::Expr)? {
                    self.bump(Mode::Expr)?;
                    if self.eat(Mode::Expr, &Tok::Colon)?.is_some() {
                        let v = self.expr(0)?;
                        a.named.push(NamedArg {
                            name: n,
                            value: v,
                            span: Span::new(s.start as usize, self.pos),
                        })
                    } else {
                        self.pos = save;
                        a.pos.push(self.expr(0)?);
                    }
                } else {
                    a.pos.push(self.expr(0)?)
                }
            }
            self.skip_newlines()?;
            if self.eat(Mode::Expr, &Tok::Comma)?.is_none() {
                self.expect(Mode::Expr, Tok::RParen, "`)`")?;
                break;
            }
            self.skip_newlines()?;
            if self.eat(Mode::Expr, &Tok::RParen)?.is_some() {
                break;
            }
        }
        Ok(a)
    }
    pub(crate) fn interp(&self, segs: Vec<Seg>, span: Span) -> ParseResult<Expr> {
        let mut parts = vec![];
        for x in segs {
            match x {
                Seg::Lit(text) => parts.push(StrPart::Lit { text }),
                Seg::Expr { start, end } => {
                    let mut parser = Parser::new(&self.lx.src[start as usize..end as usize]);
                    parser.scopes = self.scopes.clone();
                    parser.cmd_scopes = self.cmd_scopes.clone();
                    parser.repl = self.repl;
                    let mut e = parser.parse_program()?;
                    if e.stmts.len() != 1 {
                        return Err(ParseError::new(
                            "interpolation must contain one expression",
                            Span::new(start as usize, end as usize),
                        ));
                    }
                    match e.stmts.remove(0) {
                        Stmt::Expr { expr, .. } => parts.push(StrPart::Expr { expr }),
                        _ => {
                            return Err(ParseError::new(
                                "interpolation must be an expression",
                                Span::new(start as usize, end as usize),
                            ));
                        }
                    }
                }
            }
        }
        Ok(Expr::StrInterp { parts, span })
    }
}

pub(crate) fn binop(t: &Tok) -> Option<(u8, Option<BinOp>)> {
    Some(match t {
        Tok::QQ => (1, Some(BinOp::Coalesce)),
        Tok::OrOr => (2, Some(BinOp::Or)),
        Tok::AndAnd => (3, Some(BinOp::And)),
        Tok::EqEq => (4, Some(BinOp::Eq)),
        Tok::NotEq => (4, Some(BinOp::Ne)),
        Tok::Lt => (4, Some(BinOp::Lt)),
        Tok::Le => (4, Some(BinOp::Le)),
        Tok::Gt => (4, Some(BinOp::Gt)),
        Tok::Ge => (4, Some(BinOp::Ge)),
        Tok::Ident(x) if x == "in" => (4, Some(BinOp::In)),
        Tok::DotDot | Tok::DotDotEq => (5, None),
        Tok::Plus => (6, Some(BinOp::Add)),
        Tok::Minus => (6, Some(BinOp::Sub)),
        Tok::Star => (7, Some(BinOp::Mul)),
        Tok::Slash => (7, Some(BinOp::Div)),
        Tok::Percent => (7, Some(BinOp::Rem)),
        _ => return None,
    })
}

pub(crate) fn is_comparison_token(t: &Tok) -> bool {
    matches!(
        t,
        Tok::EqEq | Tok::NotEq | Tok::Lt | Tok::Le | Tok::Gt | Tok::Ge
    ) || matches!(t, Tok::Ident(x) if x == "in")
}
