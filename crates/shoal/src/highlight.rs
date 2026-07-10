use reedline::{Highlighter, StyledText};
use nu_ansi_term::{Style, Color};
use shoal_syntax::{Lexer, Mode, Tok};

pub struct ShoalHighlighter;

impl Highlighter for ShoalHighlighter {
    fn highlight(&self, line: &str, _cursor: usize) -> StyledText {
        let mut styled = StyledText::new();
        let lx = Lexer::new(line);
        let mut pos = 0;
        
        loop {
            let next_pos = lx.skip_trivia(pos);
            if next_pos > pos {
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
                    let style = match tok {
                        Tok::Int(_) | Tok::Float(_) | Tok::Size(_) | Tok::Duration(_) => Style::new().fg(Color::Cyan),
                        Tok::Str(_) | Tok::StrInterp(_) => Style::new().fg(Color::Yellow),
                        Tok::Regex(_) => Style::new().fg(Color::LightMagenta),
                        Tok::DateTime(_) | Tok::Time{..} => Style::new().fg(Color::LightBlue),
                        Tok::Ident(ref s) => {
                            match s.as_str() {
                                "let" | "var" | "fn" | "if" | "else" | "match" | "for" | "in" | "while" | "return" | "break" | "continue" | "try" | "catch" | "alias" | "use" | "export" => Style::new().fg(Color::Green).bold(),
                                "true" | "false" | "null" => Style::new().fg(Color::LightCyan),
                                _ => Style::new().fg(Color::LightBlue),
                            }
                        }
                        Tok::Eq | Tok::PlusEq | Tok::MinusEq | Tok::StarEq | Tok::SlashEq => Style::new().fg(Color::Red),
                        _ => Style::default(),
                    };
                    styled.push((style, line[start..end].to_string()));
                    pos = end;
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
