use reedline::{Highlighter, StyledText};
use nu_ansi_term::{Style, Color};
use shoal_syntax::{Lexer, Mode, Tok};

pub struct ShoalHighlighter;

fn is_valid_command(cmd: &str) -> bool {
    let builtins = ["cd", "pwd", "ls", "echo", "run", "spawn", "parallel", "jobs", "history", "clear", "exit"];
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

impl Highlighter for ShoalHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut styled = StyledText::new();
        let lx = Lexer::new(line);
        let mut pos = 0;
        let mut expect_cmd = true;
        
        loop {
            let next_pos = lx.skip_trivia(pos);
            if next_pos > pos {
                if line[pos..next_pos].contains('\n') {
                    expect_cmd = true;
                }
                styled.push((Style::default(), line[pos..next_pos].to_string()));
                pos = next_pos;
            }
            if pos >= line.len() { break; }
            
            match lx.token(pos, Mode::Expr) {
                Ok((tok, span)) => {
                    let start = span.start as usize;
                    let end = span.end as usize;
                    
                    if start > pos {
                        styled.push((Style::default(), line[pos..start].to_string()));
                    }
                    
                    if let Tok::Eof = tok { break; }
                    
                    let mut next_expect = false;
                    let style = match tok {
                        Tok::Int(_) | Tok::Float(_) | Tok::Size(_) | Tok::Duration(_) => Style::new().fg(Color::Cyan),
                        Tok::Str(_) | Tok::StrInterp(_) => Style::new().fg(Color::Yellow),
                        Tok::Regex(_) => Style::new().fg(Color::LightMagenta),
                        Tok::DateTime(_) | Tok::Time{..} => Style::new().fg(Color::LightBlue),
                        Tok::Semi | Tok::LBrace | Tok::Pipe | Tok::Caret => {
                            next_expect = true;
                            Style::default()
                        }
                        Tok::LParen | Tok::LBracket | Tok::RParen | Tok::RBracket | Tok::RBrace => {
                            Style::default().fg(Color::DarkGray)
                        }
                        Tok::Ident(ref s) => {
                            match s.as_str() {
                                "let" | "var" | "fn" | "if" | "else" | "match" | "for" | "in" | "while" | "return" | "break" | "continue" | "try" | "catch" | "alias" | "use" | "export" => {
                                    Style::new().fg(Color::Green).bold()
                                },
                                "true" | "false" | "null" => Style::new().fg(Color::LightCyan),
                                _ => {
                                    let rest = line[end..].trim_start();
                                    let is_assign = rest.starts_with('=') || rest.starts_with("+=") || rest.starts_with("-=") || rest.starts_with("*=") || rest.starts_with("/=");
                                    if is_assign {
                                        Style::new().fg(Color::LightBlue)
                                    } else if expect_cmd {
                                        if is_valid_command(s) {
                                            Style::new().fg(Color::Green)
                                        } else {
                                            Style::new().fg(Color::Red).bold()
                                        }
                                    } else {
                                        Style::new().fg(Color::LightBlue)
                                    }
                                }
                            }
                        }
                        Tok::Eq | Tok::PlusEq | Tok::MinusEq | Tok::StarEq | Tok::SlashEq => Style::new().fg(Color::Red),
                        Tok::Plus | Tok::Minus | Tok::Star | Tok::Slash | Tok::Percent | Tok::EqEq | Tok::NotEq | Tok::Lt | Tok::Le | Tok::Gt | Tok::Ge | Tok::AndAnd | Tok::OrOr => Style::new().fg(Color::LightMagenta),
                        _ => Style::default(),
                    };
                    styled.push((style, line[start..end].to_string()));
                    pos = end;
                    expect_cmd = next_expect;
                }
                Err(_) => {
                    break;
                }
            }
        }
        if pos < line.len() {
            styled.push((Style::default(), line[pos..].to_string()));
        }
        styled
    }
}
